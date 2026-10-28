//! The rewirer: which changeset each changeset spec of a new bulk change spec goes to, and what an
//! apply does to it.
//!
//! [`mappings`] pairs the specs with the changesets the bulk change has (or, for imports, any
//! changeset of the same merge request):
//!
//! - A branch spec matches a changeset the bulk change created, in the same repository, whose
//!   current spec has the same head branch.
//! - An import (`existing`) spec matches any changeset with the same repository and external
//!   id.
//! - A changeset the bulk change tracks that no spec matches (and that is not archived or
//!   being detached already) is left over: the apply removes it.
//!
//! [`Rewire::of`] says what the apply does with a mapping: create a changeset, point an
//! existing one at the new spec, attach an imported one, or remove one (archive a published
//! changeset the bulk change created, which closes it, and detach anything else).
//! [`Rewire::wanted`] is the changeset as the apply leaves it, and [`preview`] plans it with
//! [`plan_with_current`], so the preview shows what the reconciler will do after the apply.

use std::collections::{BTreeMap, HashMap};

use chrono::Utc;
use sourcegull_batches::store::{
    BatchChangeAssoc, Changeset, ChangesetSpec, ChangesetSpecType, ChangesetState,
    PublicationState, ReconcilerState, UiPublicationState,
};
use sourcegull_types::RepoId;

use crate::plan::{Plan, plan_with_current};

/// The id previews use for a bulk change that the apply would create.
pub const NEW_BATCH_CHANGE: i64 = 0;

/// A changeset spec of the bulk change spec and the changeset it goes to (either may be missing,
/// not both).
#[derive(Debug, Clone, PartialEq)]
pub struct Mapping {
    pub spec: Option<ChangesetSpec>,
    pub changeset: Option<Changeset>,
}

impl Mapping {
    /// The repository of the spec, else of the changeset.
    pub fn repo_id(&self) -> RepoId {
        match (&self.spec, &self.changeset) {
            (Some(s), _) => s.repo_id,
            (None, Some(c)) => c.repo_id,
            (None, None) => RepoId(0),
        }
    }
}

/// Whether `c` is tracked by `batch_change_id` with no pending detach and not archived.
fn tracked_by(c: &Changeset, batch_change_id: i64) -> bool {
    c.batch_change_ids
        .get(&batch_change_id)
        .is_some_and(|a| !a.detach && !a.archive && !a.is_archived)
}

/// Pairs `specs` (in order) with `candidates`: the changesets tracked or created by the batch
/// change (`batch_change_id`, `None` before its first apply) and those matching an import
/// spec. `current_specs` holds the current specs of the candidates (their head branches).
/// Specs come first, in their order, then the left-over changesets in id order.
pub fn mappings(
    batch_change_id: Option<i64>,
    specs: Vec<ChangesetSpec>,
    candidates: Vec<Changeset>,
    current_specs: &HashMap<i64, ChangesetSpec>,
) -> Vec<Mapping> {
    let mut left: Vec<Option<Changeset>> = candidates.into_iter().map(Some).collect();
    let mut out = Vec::with_capacity(specs.len() + left.len());
    for spec in specs {
        let found = left.iter().position(|slot| {
            slot.as_ref().is_some_and(|c| match spec.kind {
                ChangesetSpecType::Branch => {
                    batch_change_id.is_some()
                        && c.owned_by_batch_change_id == batch_change_id
                        && c.repo_id == spec.repo_id
                        && c.current_spec_id
                            .and_then(|id| current_specs.get(&id))
                            .is_some_and(|cur| {
                                cur.kind == ChangesetSpecType::Branch
                                    && cur.head_ref == spec.head_ref
                            })
                }
                ChangesetSpecType::Existing => {
                    c.repo_id == spec.repo_id
                        && c.external_id.is_some()
                        && c.external_id == spec.external_id
                }
            })
        });
        let changeset = found.and_then(|i| left[i].take());
        out.push(Mapping {
            spec: Some(spec),
            changeset,
        });
    }
    if let Some(bc) = batch_change_id {
        out.extend(
            left.into_iter()
                .flatten()
                .filter(|c| tracked_by(c, bc))
                .map(|c| Mapping {
                    spec: None,
                    changeset: Some(c),
                }),
        );
    }
    out
}

/// What an apply does with one [`Mapping`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rewire {
    /// Create a changeset for a branch spec, owned by the bulk change.
    CreateBranch,
    /// Create a changeset for an import spec (tracked, not owned).
    CreateImport,
    /// Point the bulk change's changeset at the new branch spec (re-attaching it if needed)
    /// and queue it.
    Update,
    /// Track an existing changeset of an import spec; `enqueue` when it was not tracked
    /// before (or its import failed).
    Attach { enqueue: bool },
    /// Archive a published changeset the bulk change created, closing it if it is open.
    Archive,
    /// Detach the changeset from the bulk change.
    Detach,
}

impl Rewire {
    /// The rewiring of `m` in bulk change `batch_change_id`.
    pub fn of(m: &Mapping, batch_change_id: i64) -> Self {
        match (&m.spec, &m.changeset) {
            (Some(s), None) => match s.kind {
                ChangesetSpecType::Branch => Self::CreateBranch,
                ChangesetSpecType::Existing => Self::CreateImport,
            },
            (Some(s), Some(c)) => match s.kind {
                ChangesetSpecType::Branch => Self::Update,
                ChangesetSpecType::Existing => Self::Attach {
                    enqueue: !tracked_by(c, batch_change_id)
                        || matches!(c.reconciler_state, ReconcilerState::Failed),
                },
            },
            (None, Some(c)) => {
                if c.owned_by_batch_change_id == Some(batch_change_id)
                    && c.publication_state == PublicationState::Published
                {
                    Self::Archive
                } else {
                    Self::Detach
                }
            }
            (None, None) => Self::Detach,
        }
    }

    /// Whether an archive also closes `c` (mirrors `ApplyTx::archive`): published and not
    /// closed, merged or deleted already.
    pub fn closes(c: &Changeset) -> bool {
        c.publication_state == PublicationState::Published
            && !matches!(
                c.state,
                ChangesetState::Closed | ChangesetState::Merged | ChangesetState::Deleted
            )
    }

    /// The changeset of `m` as the apply leaves it in bulk change `batch_change_id`.
    /// `external_service_type` is the code host type of the spec's repository (for new
    /// changesets) and `ui` the publication state chosen in the UI for the spec, if any.
    pub fn wanted(
        self,
        m: &Mapping,
        batch_change_id: i64,
        external_service_type: &str,
        ui: Option<UiPublicationState>,
    ) -> Option<Changeset> {
        let attach = |c: &mut Changeset| {
            c.batch_change_ids
                .insert(batch_change_id, BatchChangeAssoc::default());
            c.detached_at = None;
        };
        let requeue = |c: &mut Changeset| {
            c.reconciler_state = ReconcilerState::Queued;
            c.state = ChangesetState::Processing;
            c.num_failures = 0;
            c.failure_message = None;
        };
        Some(match self {
            Self::CreateBranch => {
                let spec = m.spec.as_ref()?;
                let mut c = new_changeset(spec.repo_id, external_service_type);
                attach(&mut c);
                c.owned_by_batch_change_id = Some(batch_change_id);
                c.current_spec_id = Some(spec.id);
                c.ui_publication_state = ui;
                c
            }
            Self::CreateImport => {
                let spec = m.spec.as_ref()?;
                let mut c = new_changeset(spec.repo_id, external_service_type);
                attach(&mut c);
                c.external_id = spec.external_id.clone();
                c
            }
            Self::Update => {
                let (spec, mut c) = (m.spec.as_ref()?, m.changeset.clone()?);
                if c.reconciler_state == ReconcilerState::Completed {
                    c.previous_spec_id = c.current_spec_id;
                }
                c.current_spec_id = Some(spec.id);
                c.closing = false;
                if ui.is_some() {
                    c.ui_publication_state = ui;
                }
                attach(&mut c);
                requeue(&mut c);
                c
            }
            Self::Attach { enqueue } => {
                let mut c = m.changeset.clone()?;
                attach(&mut c);
                if enqueue {
                    requeue(&mut c);
                }
                c
            }
            Self::Archive => {
                let mut c = m.changeset.clone()?;
                if Self::closes(&c) {
                    c.closing = true;
                }
                if let Some(a) = c.batch_change_ids.get_mut(&batch_change_id) {
                    a.archive = true;
                }
                requeue(&mut c);
                c
            }
            Self::Detach => {
                let mut c = m.changeset.clone()?;
                if let Some(a) = c.batch_change_ids.get_mut(&batch_change_id) {
                    a.detach = true;
                }
                requeue(&mut c);
                c
            }
        })
    }
}

/// An unpublished changeset of `repo` that is not stored yet.
fn new_changeset(repo_id: RepoId, external_service_type: &str) -> Changeset {
    let now = Utc::now();
    Changeset {
        id: 0,
        repo_id,
        batch_change_ids: BTreeMap::new(),
        owned_by_batch_change_id: None,
        current_spec_id: None,
        previous_spec_id: None,
        publication_state: PublicationState::Unpublished,
        ui_publication_state: None,
        reconciler_state: ReconcilerState::Queued,
        state: ChangesetState::Processing,
        num_failures: 0,
        failure_message: None,
        process_after: None,
        started_at: None,
        finished_at: None,
        execution_logs: Vec::new(),
        external_id: None,
        external_service_type: external_service_type.to_owned(),
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

/// The preview of one mapping: the reconciler's plan for the changeset after the apply.
/// `specs` holds the changesets' current and previous specs by id.
pub fn preview(
    m: &Mapping,
    batch_change_id: i64,
    external_service_type: &str,
    ui: Option<UiPublicationState>,
    specs: &HashMap<i64, ChangesetSpec>,
) -> Plan {
    let rewire = Rewire::of(m, batch_change_id);
    let Some(wanted) = rewire.wanted(m, batch_change_id, external_service_type, ui) else {
        return Plan::default();
    };
    let spec_of = |id: Option<i64>| id.and_then(|id| specs.get(&id));
    let (previous, current) = match rewire {
        // The new spec, compared with what the changeset was last reconciled with.
        Rewire::CreateBranch | Rewire::Update => {
            (spec_of(wanted.previous_spec_id), m.spec.as_ref())
        }
        _ => (
            spec_of(wanted.previous_spec_id),
            spec_of(wanted.current_spec_id),
        ),
    };
    plan_with_current(previous, current, m.changeset.as_ref(), &wanted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Operation as O;
    use sourcegull_batches::store::{ExternalState, Published};

    const BC: i64 = 7;

    fn spec(id: i64, repo: i32, kind: ChangesetSpecType, head: &str) -> ChangesetSpec {
        let now = Utc::now();
        ChangesetSpec {
            id,
            rand_id: format!("r{id}"),
            batch_spec_id: Some(1),
            repo_id: RepoId(repo),
            user_id: None,
            kind,
            external_id: (kind == ChangesetSpecType::Existing).then(|| head.to_owned()),
            base_ref: Some("refs/heads/main".into()),
            base_rev: Some("a".repeat(40)),
            head_ref: (kind == ChangesetSpecType::Branch).then(|| format!("refs/heads/{head}")),
            title: Some("Fix".into()),
            body: Some("Body".into()),
            published: Some(Published::Bool(true)),
            diff: Some(b"diff".to_vec()),
            diff_stat_added: Some(1),
            diff_stat_deleted: Some(0),
            commit_message: Some("Fix it".into()),
            commit_author_name: Some("A".into()),
            commit_author_email: Some("a@example.com".into()),
            fork_namespace: None,
            created_at: now,
            updated_at: now,
        }
    }

    fn owned(id: i64, repo: i32, spec_id: i64, published: bool) -> Changeset {
        let mut c = new_changeset(RepoId(repo), "gitlab");
        c.id = id;
        c.batch_change_ids.insert(BC, BatchChangeAssoc::default());
        c.owned_by_batch_change_id = Some(BC);
        c.current_spec_id = Some(spec_id);
        c.reconciler_state = ReconcilerState::Completed;
        if published {
            c.publication_state = PublicationState::Published;
            c.external_state = Some(ExternalState::Open);
            c.external_id = Some(format!("{id}"));
            c.state = ChangesetState::Open;
        } else {
            c.state = ChangesetState::Unpublished;
        }
        c
    }

    #[test]
    fn maps_branches_imports_and_leftovers() {
        let old = spec(1, 1, ChangesetSpecType::Branch, "fix");
        let current = HashMap::from([
            (1, old.clone()),
            (2, spec(2, 2, ChangesetSpecType::Branch, "fix")),
        ]);
        let mut imported = new_changeset(RepoId(3), "gitlab");
        imported.id = 30;
        imported.external_id = Some("42".into());
        let candidates = vec![owned(10, 1, 1, true), owned(20, 2, 2, false), imported];
        let new_specs = vec![
            spec(11, 1, ChangesetSpecType::Branch, "fix"),
            spec(12, 1, ChangesetSpecType::Branch, "other"),
            spec(13, 3, ChangesetSpecType::Existing, "42"),
        ];
        let ms = mappings(Some(BC), new_specs, candidates, &current);
        let got: Vec<_> = ms
            .iter()
            .map(|m| {
                (
                    m.spec.as_ref().map(|s| s.id),
                    m.changeset.as_ref().map(|c| c.id),
                    Rewire::of(m, BC),
                )
            })
            .collect();
        assert_eq!(
            got,
            vec![
                (Some(11), Some(10), Rewire::Update),
                (Some(12), None, Rewire::CreateBranch),
                (Some(13), Some(30), Rewire::Attach { enqueue: true }),
                (None, Some(20), Rewire::Detach),
            ]
        );

        // Before the first apply, branch specs match nothing.
        let ms = mappings(
            None,
            vec![spec(11, 1, ChangesetSpecType::Branch, "fix")],
            vec![owned(10, 1, 1, true)],
            &current,
        );
        assert_eq!(ms.len(), 1);
        assert!(ms[0].changeset.is_none());
    }

    #[test]
    fn previews() {
        let old = spec(1, 1, ChangesetSpecType::Branch, "fix");
        let specs = HashMap::from([(1, old.clone())]);
        // New: push and publish.
        let new = Mapping {
            spec: Some(spec(11, 1, ChangesetSpecType::Branch, "fix")),
            changeset: None,
        };
        assert_eq!(
            preview(&new, BC, "gitlab", None, &specs).ops,
            vec![O::Push, O::Publish]
        );
        // Unset `published`: the UI decides.
        let mut ui = new.clone();
        ui.spec.as_mut().unwrap().published = None;
        assert!(preview(&ui, BC, "gitlab", None, &specs).is_empty());
        assert_eq!(
            preview(&ui, BC, "gitlab", Some(UiPublicationState::Draft), &specs).ops,
            vec![O::Push, O::PublishDraft]
        );
        // Changed title: update.
        let mut changed = spec(11, 1, ChangesetSpecType::Branch, "fix");
        changed.title = Some("New".into());
        let m = Mapping {
            spec: Some(changed),
            changeset: Some(owned(10, 1, 1, true)),
        };
        let pl = preview(&m, BC, "gitlab", None, &specs);
        assert_eq!(pl.ops, vec![O::Update]);
        assert!(pl.delta.title_changed);
        // Unchanged: nothing.
        let m = Mapping {
            spec: Some(spec(11, 1, ChangesetSpecType::Branch, "fix")),
            changeset: Some(owned(10, 1, 1, true)),
        };
        assert!(preview(&m, BC, "gitlab", None, &specs).is_empty());
        // Dropped and published: close and archive; unpublished: detach.
        let gone = Mapping {
            spec: None,
            changeset: Some(owned(10, 1, 1, true)),
        };
        assert_eq!(
            preview(&gone, BC, "gitlab", None, &specs).ops,
            vec![O::Archive, O::Close]
        );
        let gone = Mapping {
            spec: None,
            changeset: Some(owned(10, 1, 1, false)),
        };
        assert_eq!(
            preview(&gone, BC, "gitlab", None, &specs).ops,
            vec![O::Detach]
        );
        // Import.
        let imp = Mapping {
            spec: Some(spec(13, 3, ChangesetSpecType::Existing, "42")),
            changeset: None,
        };
        assert_eq!(
            preview(&imp, BC, "gitlab", None, &specs).ops,
            vec![O::Import]
        );
    }
}
