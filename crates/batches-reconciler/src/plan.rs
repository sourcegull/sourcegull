//! The planner: what to do to bring a changeset from its current state to the state its
//! changeset spec asks for.
//!
//! [`plan`] is a pure function of the changeset row (publication and external state, batch
//! change memberships with their detach and archive requests, `closing`, the UI publication
//! state) and the changeset's current and previous specs. The rules:
//!
//! - Detach and archive requests become [`Operation::Detach`] / [`Operation::Archive`]. A
//!   detach from the owning bulk change, and archiving, need nothing else.
//! - `closing` closes the changeset (unless it is read-only) and is final.
//! - A changeset without a branch spec (imported, or a spec of type `existing`) is imported
//!   while it is unpublished and still tracked.
//! - An unpublished changeset is pushed and published (as a draft when the spec or the UI
//!   says `draft` and the code host has drafts) once the spec's `published`, or the UI
//!   publication state when the spec leaves it unset, says so.
//! - A published changeset that is merged or read-only is left alone. A closed one still
//!   owned by and attached to its bulk change is reopened. Going from draft to published
//!   undrafts it. A changed diff, commit message or author is pushed; a changed title, body
//!   or base branch updates the merge request, otherwise the push is followed by a short
//!   sleep and a sync so the code host has seen the new commit.
//!
//! Operations run in a fixed order ([`Operation::precedence`]).

use sourcegull_batches::store::{
    Changeset, ChangesetSpec, ChangesetSpecType, ExternalState, PublicationState, Published,
    UiPublicationState,
};

/// One step of a [`Plan`] (GraphQL `ChangesetSpecOperation`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Operation {
    /// Commit the spec's diff to the changeset's branch and push it.
    Push,
    /// Carry out detach requests (drop the bulk changes from `batch_change_ids`).
    Detach,
    /// Carry out archive requests.
    Archive,
    /// The changeset is attached again after a detach (preview only, see
    /// [`plan_with_current`]).
    Reattach,
    /// Load an existing merge request (an imported changeset) for the first time.
    Import,
    /// Open the merge request.
    Publish,
    /// Open the merge request as a draft.
    PublishDraft,
    /// Close the merge request.
    Close,
    /// Reopen a closed merge request.
    Reopen,
    /// Mark a draft merge request ready.
    Undraft,
    /// Update title, body and base branch.
    Update,
    /// Give the code host a moment to see a pushed commit before syncing.
    Sleep,
    /// Load the merge request's state.
    Sync,
}

impl Operation {
    /// Every operation.
    pub const ALL: &'static [Self] = &[
        Self::Push,
        Self::Detach,
        Self::Archive,
        Self::Reattach,
        Self::Import,
        Self::Publish,
        Self::PublishDraft,
        Self::Close,
        Self::Reopen,
        Self::Undraft,
        Self::Update,
        Self::Sleep,
        Self::Sync,
    ];

    /// The GraphQL spelling, e.g. `PUBLISH_DRAFT`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Push => "PUSH",
            Self::Detach => "DETACH",
            Self::Archive => "ARCHIVE",
            Self::Reattach => "REATTACH",
            Self::Import => "IMPORT",
            Self::Publish => "PUBLISH",
            Self::PublishDraft => "PUBLISH_DRAFT",
            Self::Close => "CLOSE",
            Self::Reopen => "REOPEN",
            Self::Undraft => "UNDRAFT",
            Self::Update => "UPDATE",
            Self::Sleep => "SLEEP",
            Self::Sync => "SYNC",
        }
    }

    /// Execution order: lower runs first (ties keep the planner's order). Pushing comes
    /// before opening the merge request, reopening before undrafting and updating, and a
    /// sync last.
    pub const fn precedence(self) -> u8 {
        match self {
            Self::Push | Self::Detach | Self::Archive | Self::Reattach => 0,
            Self::Import | Self::Publish | Self::PublishDraft | Self::Close => 1,
            Self::Reopen => 2,
            Self::Undraft => 3,
            Self::Update => 4,
            Self::Sleep => 5,
            Self::Sync => 6,
        }
    }

    /// Whether the operation talks to the code host or a repo node.
    pub const fn is_remote(self) -> bool {
        !matches!(
            self,
            Self::Detach | Self::Archive | Self::Reattach | Self::Sleep
        )
    }
}

impl std::fmt::Display for Operation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What changed between the previous and the current changeset spec.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Delta {
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

impl Delta {
    /// The commit must be recreated and pushed.
    pub fn need_commit_update(&self) -> bool {
        self.diff_changed
            || self.commit_message_changed
            || self.author_name_changed
            || self.author_email_changed
    }

    /// The merge request's attributes must be updated.
    pub fn need_code_host_update(&self) -> bool {
        self.title_changed || self.body_changed || self.base_ref_changed
    }

    /// Anything besides the publication state changed.
    pub fn attributes_changed(&self) -> bool {
        self.need_commit_update() || self.need_code_host_update()
    }
}

/// The operations for one changeset, in execution order and without duplicates.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    pub ops: Vec<Operation>,
    pub delta: Delta,
    /// The plan publishes and rollout windows apply: the changeset was moved to `scheduled`
    /// and nothing was executed ([`Reconciler::reconcile`](crate::Reconciler::reconcile)).
    pub scheduled: bool,
}

impl Plan {
    /// Nothing to do.
    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    /// Whether the plan opens the merge request (`PUBLISH` or `PUBLISH_DRAFT`), which rollout
    /// windows gate.
    pub fn publishes(&self) -> bool {
        self.contains(Operation::Publish) || self.contains(Operation::PublishDraft)
    }

    /// Whether the plan has `op`.
    pub fn contains(&self, op: Operation) -> bool {
        self.ops.contains(&op)
    }

    fn set(&mut self, op: Operation) {
        self.ops = vec![op];
    }

    fn add(&mut self, op: Operation) {
        self.ops.push(op);
    }

    fn finish(mut self) -> Self {
        let mut seen = Vec::with_capacity(self.ops.len());
        for op in self.ops {
            if !seen.contains(&op) {
                seen.push(op);
            }
        }
        // Stable: ties keep the order they were planned in.
        seen.sort_by_key(|op| op.precedence());
        self.ops = seen;
        self
    }
}

impl std::fmt::Display for Plan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.ops.is_empty() {
            return f.write_str("no operations");
        }
        for (i, op) in self.ops.iter().enumerate() {
            if i > 0 {
                f.write_str(" => ")?;
            }
            f.write_str(op.as_str())?;
        }
        Ok(())
    }
}

/// The publication a spec asks for: its `published` field, or the UI publication state when
/// the spec leaves it unset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DesiredPublication {
    Unpublished,
    Draft,
    Published,
}

impl DesiredPublication {
    /// From a spec's `published` and the changeset's UI publication state.
    pub fn of(spec: Option<Published>, ui: Option<UiPublicationState>) -> Self {
        match (spec, ui) {
            (Some(Published::Bool(true)), _) => Self::Published,
            (Some(Published::Draft), _) => Self::Draft,
            (Some(Published::Bool(false)), _) => Self::Unpublished,
            (None, Some(UiPublicationState::Published)) => Self::Published,
            (None, Some(UiPublicationState::Draft)) => Self::Draft,
            (None, Some(UiPublicationState::Unpublished) | None) => Self::Unpublished,
        }
    }
}

/// Whether merge requests on a code host of `external_service_type` can be drafts.
pub fn supports_draft(external_service_type: &str) -> bool {
    external_service_type == "gitlab"
}

/// The spec delta between the last applied spec and the current one (none without a
/// previous spec).
pub fn compare_specs(
    previous: Option<&ChangesetSpec>,
    current: &ChangesetSpec,
    ui: Option<UiPublicationState>,
) -> Delta {
    let Some(prev) = previous else {
        return Delta::default();
    };
    Delta {
        title_changed: prev.title != current.title,
        body_changed: prev.body != current.body,
        undraft: DesiredPublication::of(prev.published, ui) == DesiredPublication::Draft
            && DesiredPublication::of(current.published, ui) == DesiredPublication::Published,
        base_ref_changed: prev.base_ref != current.base_ref,
        diff_changed: prev.diff != current.diff,
        commit_message_changed: prev.commit_message != current.commit_message,
        author_name_changed: prev.commit_author_name != current.commit_author_name,
        author_email_changed: prev.commit_author_email != current.commit_author_email,
    }
}

/// The reconciler's plan for `changeset` with its previous and current spec.
pub fn plan(
    previous: Option<&ChangesetSpec>,
    current: Option<&ChangesetSpec>,
    changeset: &Changeset,
) -> Plan {
    plan_with_current(previous, current, None, changeset)
}

/// Like [`plan`], for previews: `before` is the changeset as it is now and `wanted` as an
/// apply would leave it. A changeset that is tracked by no bulk change in `before` but is
/// attached in `wanted` gets [`Operation::Reattach`].
pub fn plan_with_current(
    previous: Option<&ChangesetSpec>,
    current: Option<&ChangesetSpec>,
    before: Option<&Changeset>,
    wanted: &Changeset,
) -> Plan {
    let mut pl = Plan::default();

    let mut want_detach = false;
    let mut want_archive = false;
    let mut is_archived = false;
    let mut still_attached = false;
    let mut reattach = false;
    let mut detach_from_owner = false;
    let published = wanted.publication_state == PublicationState::Published;
    for (&id, assoc) in &wanted.batch_change_ids {
        let owner = wanted.owned_by_batch_change_id == Some(id);
        if assoc.detach {
            want_detach = true;
            detach_from_owner |= owner;
        } else if assoc.archive && owner && published {
            want_archive = !assoc.is_archived;
            is_archived = assoc.is_archived;
        } else if owner && assoc.is_archived {
            // Archived by its owner: left alone (not reopened) until an apply attaches it
            // again.
            is_archived = true;
        } else if before.is_some_and(|b| b.batch_change_ids.is_empty()) {
            reattach = true;
        } else {
            still_attached = true;
        }
    }
    if want_detach {
        pl.set(Operation::Detach);
    }
    if want_archive {
        pl.set(Operation::Archive);
    }

    if wanted.closing {
        if wanted.external_state != Some(ExternalState::ReadOnly) {
            pl.add(Operation::Close);
        }
        return pl.finish();
    }
    if detach_from_owner || want_archive || is_archived {
        // Detached from (or archived in) its owner: no more writes. A detach from another
        // bulk change can still go with an update from the owner's spec.
        return pl.finish();
    }

    // Imported changesets have no branch spec: they are only loaded.
    let Some(current) = current.filter(|s| s.kind == ChangesetSpecType::Branch) else {
        if wanted.publication_state == PublicationState::Unpublished && still_attached {
            pl.add(Operation::Import);
        } else if reattach && !want_detach {
            pl.add(Operation::Reattach);
        }
        return pl.finish();
    };
    if previous.is_some() && reattach && !want_detach {
        pl.add(Operation::Reattach);
    }

    let delta = compare_specs(previous, current, wanted.ui_publication_state);
    pl.delta = delta;
    let desired = DesiredPublication::of(current.published, wanted.ui_publication_state);
    let drafts = supports_draft(&wanted.external_service_type);

    match wanted.publication_state {
        PublicationState::Unpublished => match desired {
            DesiredPublication::Published => {
                pl.set(Operation::Publish);
                pl.add(Operation::Push);
            }
            DesiredPublication::Draft if drafts => {
                pl.set(Operation::PublishDraft);
                pl.add(Operation::Push);
            }
            // A draft on a code host without drafts, or unpublished: nothing yet.
            _ => {}
        },
        PublicationState::Published => {
            if matches!(
                wanted.external_state,
                Some(ExternalState::Merged | ExternalState::ReadOnly)
            ) {
                return pl.finish();
            }
            if reopen_after_detach(wanted) {
                pl.set(Operation::Reopen);
            }
            if drafts
                && (delta.undraft
                    || (desired == DesiredPublication::Published
                        && wanted.external_state == Some(ExternalState::Draft)))
            {
                pl.add(Operation::Undraft);
            }
            if delta.attributes_changed() {
                if delta.need_commit_update() {
                    pl.add(Operation::Push);
                }
                if delta.need_code_host_update() {
                    pl.add(Operation::Update);
                } else {
                    // Only the commit changed: let the code host see it, then sync.
                    pl.add(Operation::Sleep);
                    pl.add(Operation::Sync);
                }
            }
        }
    }
    pl.finish()
}

/// A closed changeset that its owning bulk change still tracks (and does not close) is
/// reopened: it was detached and closed, and an apply brought it back.
fn reopen_after_detach(ch: &Changeset) -> bool {
    let closed = matches!(
        ch.external_state,
        Some(ExternalState::Closed | ExternalState::ReadOnly)
    );
    let Some(owner) = ch.owned_by_batch_change_id else {
        return false;
    };
    closed && !ch.closing && ch.batch_change_ids.contains_key(&owner)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use chrono::Utc;
    use sourcegull_batches::store::{BatchChangeAssoc, ChangesetState, ReconcilerState};
    use sourcegull_types::RepoId;

    use super::*;
    use Operation as O;

    const OWNER: i64 = 1;
    const OTHER: i64 = 2;

    fn changeset() -> Changeset {
        let now = Utc::now();
        Changeset {
            id: 1,
            repo_id: RepoId(1),
            batch_change_ids: BTreeMap::from([(OWNER, BatchChangeAssoc::default())]),
            owned_by_batch_change_id: Some(OWNER),
            current_spec_id: Some(2),
            previous_spec_id: None,
            publication_state: PublicationState::Unpublished,
            ui_publication_state: None,
            reconciler_state: ReconcilerState::Processing,
            state: ChangesetState::Processing,
            num_failures: 0,
            failure_message: None,
            process_after: None,
            started_at: None,
            finished_at: None,
            execution_logs: vec![],
            external_id: None,
            external_service_type: "gitlab".into(),
            external_branch: None,
            external_fork_namespace: None,
            external_fork_name: None,
            external_state: None,
            external_review_state: None,
            external_check_state: None,
            external_updated_at: None,
            external_deleted_at: None,
            diff_stat_added: None,
            diff_stat_deleted: None,
            metadata: serde_json::json!({}),
            title: None,
            body: None,
            sync_state: serde_json::json!({}),
            last_synced_at: None,
            next_sync_at: None,
            syncer_error: None,
            detached_at: None,
            closing: false,
            window_released: false,
            created_at: now,
            updated_at: now,
        }
    }

    fn published(external: ExternalState) -> Changeset {
        Changeset {
            publication_state: PublicationState::Published,
            external_state: Some(external),
            external_id: Some("12".into()),
            ..changeset()
        }
    }

    fn spec(published: Option<Published>) -> ChangesetSpec {
        let now = Utc::now();
        ChangesetSpec {
            id: 2,
            rand_id: "r".into(),
            batch_spec_id: Some(1),
            repo_id: RepoId(1),
            user_id: None,
            kind: ChangesetSpecType::Branch,
            external_id: None,
            base_ref: Some("refs/heads/main".into()),
            base_rev: Some("a".repeat(40)),
            head_ref: Some("refs/heads/fix".into()),
            title: Some("Fix".into()),
            body: Some("Body".into()),
            published,
            diff: Some(b"diff".to_vec()),
            diff_stat_added: Some(1),
            diff_stat_deleted: Some(1),
            commit_message: Some("Fix it".into()),
            commit_author_name: Some("A".into()),
            commit_author_email: Some("a@example.com".into()),
            fork_namespace: None,
            created_at: now,
            updated_at: now,
        }
    }

    const TRUE: Option<Published> = Some(Published::Bool(true));
    const FALSE: Option<Published> = Some(Published::Bool(false));
    const DRAFT: Option<Published> = Some(Published::Draft);

    fn assoc(detach: bool, archive: bool, is_archived: bool) -> BatchChangeAssoc {
        BatchChangeAssoc {
            detach,
            archive,
            is_archived,
        }
    }

    struct Case {
        name: &'static str,
        previous: Option<ChangesetSpec>,
        current: Option<ChangesetSpec>,
        changeset: Changeset,
        want: Vec<Operation>,
    }

    fn case(
        name: &'static str,
        previous: Option<ChangesetSpec>,
        current: Option<ChangesetSpec>,
        changeset: Changeset,
        want: &[Operation],
    ) -> Case {
        Case {
            name,
            previous,
            current,
            changeset,
            want: want.to_vec(),
        }
    }

    fn with_ui(mut c: Changeset, ui: UiPublicationState) -> Changeset {
        c.ui_publication_state = Some(ui);
        c
    }

    fn not_gitlab(mut c: Changeset) -> Changeset {
        c.external_service_type = "other".into();
        c
    }

    fn with_assocs(mut c: Changeset, assocs: &[(i64, BatchChangeAssoc)]) -> Changeset {
        c.batch_change_ids = assocs.iter().copied().collect();
        c
    }

    fn closing(mut c: Changeset) -> Changeset {
        c.closing = true;
        c
    }

    fn changed(f: impl FnOnce(&mut ChangesetSpec)) -> Option<ChangesetSpec> {
        let mut s = spec(TRUE);
        f(&mut s);
        Some(s)
    }

    fn imported() -> Changeset {
        Changeset {
            owned_by_batch_change_id: None,
            current_spec_id: None,
            external_id: Some("7".into()),
            ..changeset()
        }
    }

    #[test]
    fn plans() {
        use ExternalState as E;
        use UiPublicationState as Ui;
        let cases = vec![
            // Publication from the spec.
            case(
                "publish true",
                None,
                Some(spec(TRUE)),
                changeset(),
                &[O::Push, O::Publish],
            ),
            case(
                "publish as draft",
                None,
                Some(spec(DRAFT)),
                changeset(),
                &[O::Push, O::PublishDraft],
            ),
            case("publish false", None, Some(spec(FALSE)), changeset(), &[]),
            case(
                "draft on a code host without drafts",
                None,
                Some(spec(DRAFT)),
                not_gitlab(changeset()),
                &[],
            ),
            case(
                "draft to true on a published draft",
                Some(spec(DRAFT)),
                Some(spec(TRUE)),
                published(E::Draft),
                &[O::Undraft],
            ),
            case(
                "draft to true on an unpublished changeset",
                Some(spec(DRAFT)),
                Some(spec(TRUE)),
                changeset(),
                &[O::Push, O::Publish],
            ),
            case(
                "true to draft is ignored",
                Some(spec(TRUE)),
                Some(spec(DRAFT)),
                published(E::Open),
                &[],
            ),
            // Publication from the UI when the spec leaves it unset.
            case(
                "published unset, no UI state",
                None,
                Some(spec(None)),
                changeset(),
                &[],
            ),
            case(
                "published unset, UI unpublished",
                None,
                Some(spec(None)),
                with_ui(changeset(), Ui::Unpublished),
                &[],
            ),
            case(
                "published unset, UI draft",
                None,
                Some(spec(None)),
                with_ui(changeset(), Ui::Draft),
                &[O::Push, O::PublishDraft],
            ),
            case(
                "published unset, UI draft, no drafts",
                None,
                Some(spec(None)),
                not_gitlab(with_ui(changeset(), Ui::Draft)),
                &[],
            ),
            case(
                "published unset, UI published",
                None,
                Some(spec(None)),
                with_ui(changeset(), Ui::Published),
                &[O::Push, O::Publish],
            ),
            case(
                "spec draft to unset, UI published",
                Some(spec(DRAFT)),
                Some(spec(None)),
                with_ui(published(E::Draft), Ui::Published),
                &[O::Undraft],
            ),
            case(
                "spec draft to unset, UI draft",
                Some(spec(DRAFT)),
                Some(spec(None)),
                with_ui(published(E::Draft), Ui::Draft),
                &[],
            ),
            case(
                "spec draft to unset, UI unpublished",
                Some(spec(DRAFT)),
                Some(spec(None)),
                with_ui(published(E::Draft), Ui::Unpublished),
                &[],
            ),
            case(
                "spec draft to unset, no UI state",
                Some(spec(DRAFT)),
                Some(spec(None)),
                published(E::Draft),
                &[],
            ),
            case(
                "spec true to unset, no UI state",
                Some(spec(TRUE)),
                Some(spec(None)),
                published(E::Open),
                &[],
            ),
            case(
                "UI draft to UI published on a draft",
                Some(spec(None)),
                Some(spec(None)),
                with_ui(published(E::Draft), Ui::Published),
                &[O::Undraft],
            ),
            case(
                "UI published to UI draft on an open changeset",
                Some(spec(None)),
                Some(spec(None)),
                with_ui(published(E::Open), Ui::Draft),
                &[],
            ),
            case(
                "UI published on a draft without drafts support",
                Some(spec(None)),
                Some(spec(None)),
                not_gitlab(with_ui(published(E::Draft), Ui::Published)),
                &[],
            ),
            // Spec changes on published changesets.
            case(
                "published read-only changeset",
                Some(spec(TRUE)),
                changed(|s| s.title = Some("New".into())),
                published(E::ReadOnly),
                &[],
            ),
            case(
                "nothing changed",
                Some(spec(TRUE)),
                Some(spec(TRUE)),
                published(E::Open),
                &[],
            ),
            case(
                "title changed",
                Some(spec(TRUE)),
                changed(|s| s.title = Some("New".into())),
                published(E::Open),
                &[O::Update],
            ),
            case(
                "body changed",
                Some(spec(TRUE)),
                changed(|s| s.body = Some("New".into())),
                published(E::Open),
                &[O::Update],
            ),
            case(
                "base ref changed",
                Some(spec(TRUE)),
                changed(|s| s.base_ref = Some("refs/heads/dev".into())),
                published(E::Open),
                &[O::Update],
            ),
            case(
                "diff changed",
                Some(spec(TRUE)),
                changed(|s| s.diff = Some(b"other".to_vec())),
                published(E::Open),
                &[O::Push, O::Sleep, O::Sync],
            ),
            case(
                "commit message changed",
                Some(spec(TRUE)),
                changed(|s| s.commit_message = Some("Other".into())),
                published(E::Open),
                &[O::Push, O::Sleep, O::Sync],
            ),
            case(
                "author name changed",
                Some(spec(TRUE)),
                changed(|s| s.commit_author_name = Some("B".into())),
                published(E::Open),
                &[O::Push, O::Sleep, O::Sync],
            ),
            case(
                "author email changed",
                Some(spec(TRUE)),
                changed(|s| s.commit_author_email = Some("b@example.com".into())),
                published(E::Open),
                &[O::Push, O::Sleep, O::Sync],
            ),
            case(
                "diff and title changed",
                Some(spec(TRUE)),
                changed(|s| {
                    s.diff = Some(b"other".to_vec());
                    s.title = Some("New".into());
                }),
                published(E::Open),
                &[O::Push, O::Update],
            ),
            case(
                "diff changed on a merged changeset",
                Some(spec(TRUE)),
                changed(|s| s.diff = Some(b"other".to_vec())),
                published(E::Merged),
                &[],
            ),
            case(
                "title and undraft on a draft",
                Some(spec(DRAFT)),
                changed(|s| s.title = Some("New".into())),
                published(E::Draft),
                &[O::Undraft, O::Update],
            ),
            // Reopening.
            case(
                "closed and re-attached to its owner is reopened",
                Some(spec(TRUE)),
                changed(|s| s.title = Some("New".into())),
                published(E::Closed),
                &[O::Reopen, O::Update],
            ),
            case(
                "closed without changes is reopened",
                Some(spec(TRUE)),
                Some(spec(TRUE)),
                published(E::Closed),
                &[O::Reopen],
            ),
            case(
                "closed changeset no longer attached to its owner stays closed",
                Some(spec(TRUE)),
                Some(spec(TRUE)),
                with_assocs(published(E::Closed), &[(OTHER, assoc(false, false, false))]),
                &[],
            ),
            // Closing.
            case(
                "closing an open changeset",
                Some(spec(TRUE)),
                changed(|s| s.title = Some("New".into())),
                closing(published(E::Open)),
                &[O::Close],
            ),
            case(
                "closing an already closed changeset",
                Some(spec(TRUE)),
                Some(spec(TRUE)),
                closing(published(E::Closed)),
                &[O::Close],
            ),
            case(
                "closing a read-only changeset",
                Some(spec(TRUE)),
                Some(spec(TRUE)),
                closing(published(E::ReadOnly)),
                &[],
            ),
            case(
                "closing and detaching",
                Some(spec(TRUE)),
                Some(spec(TRUE)),
                closing(with_assocs(
                    published(E::Open),
                    &[(OWNER, assoc(true, false, false))],
                )),
                &[O::Detach, O::Close],
            ),
            // Detaching and archiving.
            case(
                "detaching from the owner",
                Some(spec(TRUE)),
                changed(|s| s.title = Some("New".into())),
                with_assocs(published(E::Closed), &[(OWNER, assoc(true, false, false))]),
                &[O::Detach],
            ),
            case(
                "detaching from another bulk change still updates",
                Some(spec(TRUE)),
                changed(|s| s.title = Some("New".into())),
                with_assocs(
                    published(E::Open),
                    &[
                        (OWNER, assoc(false, false, false)),
                        (OTHER, assoc(true, false, false)),
                    ],
                ),
                &[O::Detach, O::Update],
            ),
            case(
                "detaching an unpublished changeset that failed to publish",
                None,
                Some(spec(TRUE)),
                with_assocs(changeset(), &[(OWNER, assoc(true, false, false))]),
                &[O::Detach],
            ),
            case(
                "detaching an importing changeset",
                None,
                None,
                with_assocs(imported(), &[(OWNER, assoc(true, false, false))]),
                &[O::Detach],
            ),
            case(
                "archiving",
                Some(spec(TRUE)),
                Some(spec(TRUE)),
                with_assocs(published(E::Closed), &[(OWNER, assoc(false, true, false))]),
                &[O::Archive],
            ),
            case(
                "already archived",
                Some(spec(TRUE)),
                changed(|s| s.title = Some("New".into())),
                with_assocs(published(E::Closed), &[(OWNER, assoc(false, true, true))]),
                &[],
            ),
            case(
                "archived changeset queued again is not reopened",
                Some(spec(TRUE)),
                Some(spec(TRUE)),
                with_assocs(published(E::Closed), &[(OWNER, assoc(false, false, true))]),
                &[],
            ),
            case(
                "archive request on an unpublished changeset is ignored",
                None,
                Some(spec(FALSE)),
                with_assocs(changeset(), &[(OWNER, assoc(false, true, false))]),
                &[],
            ),
            // Imports.
            case(
                "import",
                None,
                None,
                with_assocs(imported(), &[(OWNER, assoc(false, false, false))]),
                &[O::Import],
            ),
            case(
                "import by an existing-type spec",
                None,
                Some(ChangesetSpec {
                    kind: ChangesetSpecType::Existing,
                    external_id: Some("7".into()),
                    ..spec(None)
                }),
                with_assocs(imported(), &[(OWNER, assoc(false, false, false))]),
                &[O::Import],
            ),
            case(
                "imported and already published",
                None,
                None,
                Changeset {
                    publication_state: PublicationState::Published,
                    ..with_assocs(imported(), &[(OWNER, assoc(false, false, false))])
                },
                &[],
            ),
            case(
                "detached from one, still imported by another",
                None,
                None,
                with_assocs(
                    imported(),
                    &[
                        (OWNER, assoc(true, false, false)),
                        (OTHER, assoc(false, false, false)),
                    ],
                ),
                &[O::Detach, O::Import],
            ),
        ];
        for c in cases {
            let got = plan(c.previous.as_ref(), c.current.as_ref(), &c.changeset);
            assert_eq!(got.ops, c.want, "{}: got {got}", c.name);
        }
    }

    #[test]
    fn reattach_in_previews() {
        let before = with_assocs(published(ExternalState::Open), &[]);
        let wanted = published(ExternalState::Open);
        let got = plan_with_current(Some(&spec(TRUE)), Some(&spec(TRUE)), Some(&before), &wanted);
        assert_eq!(got.ops, vec![O::Reattach]);
        // An imported changeset tracked again.
        let wanted = Changeset {
            publication_state: PublicationState::Published,
            ..imported()
        };
        let before = with_assocs(wanted.clone(), &[]);
        let got = plan_with_current(None, None, Some(&before), &wanted);
        assert_eq!(got.ops, vec![O::Reattach]);
        // The reconciler itself never plans it.
        assert!(plan(Some(&spec(TRUE)), Some(&spec(TRUE)), &wanted).is_empty());
    }

    #[test]
    fn deltas() {
        let base = spec(TRUE);
        assert_eq!(compare_specs(None, &base, None), Delta::default());
        assert_eq!(compare_specs(Some(&base), &base, None), Delta::default());
        let d = compare_specs(Some(&spec(DRAFT)), &base, None);
        assert!(d.undraft && !d.attributes_changed());
        let d = compare_specs(
            Some(&spec(None)),
            &spec(None),
            Some(UiPublicationState::Published),
        );
        assert!(!d.undraft);
        let mut other = spec(TRUE);
        other.diff = None;
        let d = compare_specs(Some(&base), &other, None);
        assert!(d.diff_changed && d.need_commit_update() && !d.need_code_host_update());
    }

    #[test]
    fn desired_publication() {
        use DesiredPublication as D;
        use UiPublicationState as Ui;
        assert_eq!(D::of(TRUE, Some(Ui::Unpublished)), D::Published);
        assert_eq!(D::of(FALSE, Some(Ui::Published)), D::Unpublished);
        assert_eq!(D::of(DRAFT, None), D::Draft);
        assert_eq!(D::of(None, None), D::Unpublished);
        assert_eq!(D::of(None, Some(Ui::Draft)), D::Draft);
        assert_eq!(D::of(None, Some(Ui::Published)), D::Published);
    }

    #[test]
    fn execution_order_and_display() {
        let p = Plan {
            ops: vec![O::Sync, O::Update, O::Push, O::Sync, O::Reopen],
            ..Plan::default()
        }
        .finish();
        assert_eq!(p.ops, vec![O::Push, O::Reopen, O::Update, O::Sync]);
        assert_eq!(p.to_string(), "PUSH => REOPEN => UPDATE => SYNC");
        assert_eq!(Plan::default().to_string(), "no operations");
        for op in Operation::ALL {
            assert!(!op.as_str().is_empty());
        }
    }
}
