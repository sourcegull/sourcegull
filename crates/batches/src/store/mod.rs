//! Bulk changes stores.
//!
//! - [`BatchSpecStore`]: `batch_specs` and `batch_spec_resolution_jobs` (workspace resolution
//!   queue).
//! - [`BatchSpecWorkspaceStore`]: `batch_spec_workspaces` and
//!   `batch_spec_workspace_execution_jobs` (the `batches` executor queue, dequeued fairly
//!   between users).
//! - [`ExecutionCacheStore`]: `batch_spec_execution_cache_entries`, step results by cache key.
//! - [`BatchSpecWorkspaceFileStore`]: `batch_spec_workspace_files`, files server-side specs
//!   mount.
//! - [`ChangesetSpecStore`]: the `changeset_specs` table.
//! - [`BatchChangeStore`]: `batch_changes`, plus [`ApplyTx`], the transaction an apply runs
//!   in (attach, detach, archive and re-enqueue changesets).
//! - [`ChangesetStore`]: `changesets` (the reconciler queue) and `changeset_events`.
//! - [`ChangesetJobStore`]: `changeset_jobs` (bulk operations queue).
//! - [`burndown`]: the burndown chart, daily changeset counts rebuilt from the events.
//! - [`BatchesCredentialStore`]: `user_credentials` and `site_credentials`, encrypted with the
//!   `user_credential` and `site_credential` data keys.
//!
//! The UI state of a changeset ([`ChangesetState`]) is computed from its reconciler,
//! publication and external states by [`compute_changeset_state`] and stored in the generated
//! column `changesets.computed_state` (same logic in SQL, `changeset_computed_state`), so
//! listings filter and count by it with an index.

mod batch_changes;
pub mod burndown;
mod changeset_jobs;
mod changeset_specs;
mod changesets;
mod credentials;
mod files;
mod specs;
mod workspaces;

pub use batch_changes::{
    ApplyTx, BatchChange, BatchChangeState, BatchChangeStore, ListBatchChanges, NewBatchChange,
};
pub use changeset_jobs::{
    BulkOperation, CHANGESET_JOBS_TABLE, ChangesetJob, ChangesetJobRow, ChangesetJobStore,
    ChangesetJobType,
};
pub use changeset_specs::{
    CHANGESET_SPEC_TTL, ChangesetSpec, ChangesetSpecStore, ChangesetSpecType, ListChangesetSpecs,
    NewChangesetSpec, Published,
};
pub use changesets::{
    ArchivedFilter, BatchChangeAssoc, CHANGESETS_TABLE, Changeset, ChangesetCounts, ChangesetEvent,
    ChangesetHookUpdate, ChangesetStore, ChangesetSync, ListChangesets, NewChangeset,
    NewChangesetEvent,
};
pub use credentials::{
    BatchesCredential, BatchesCredentialStore, CredentialKind, CredentialSecret, SshKeypair,
};
pub use files::{BatchSpecWorkspaceFile, BatchSpecWorkspaceFileStore, NewBatchSpecWorkspaceFile};
pub use specs::{
    BATCH_SPEC_RESOLUTION_JOBS_TABLE, BATCH_SPEC_TTL, BatchSpec, BatchSpecStore, ListBatchSpecs,
    NewBatchSpec, ResolutionJob, ResolutionJobRow,
};
pub use workspaces::{
    BATCH_SPEC_EXECUTION_JOBS_TABLE, BatchSpecWorkspace, BatchSpecWorkspaceStore,
    EXECUTION_JOB_VERSION, ExecutionCacheEntry, ExecutionCacheStore, ExecutionJob, ExecutionJobRow,
    ExecutionStats, ListWorkspaces, NewWorkspace,
};

/// Defines a string-backed enum with `as_str` and `parse` over its stored values.
macro_rules! str_enum {
    ($(#[$meta:meta])* $name:ident { $($(#[$vmeta:meta])* $variant:ident => $s:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum $name {
            $($(#[$vmeta])* $variant),+
        }

        impl $name {
            /// Every value.
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];

            /// The stored value.
            pub const fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $s),+
                }
            }

            /// Parses a stored value.
            pub fn parse(s: &str) -> Option<Self> {
                match s {
                    $($s => Some(Self::$variant),)+
                    _ => None,
                }
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }
    };
}
pub(crate) use str_enum;

str_enum!(
    /// The reconciler's state of a changeset, derived from the queue columns
    /// ([`ReconcilerState::from_queue`]).
    ReconcilerState {
        /// Waiting for a rollout window before being queued.
        Scheduled => "SCHEDULED",
        Queued => "QUEUED",
        Processing => "PROCESSING",
        /// The last attempt failed; it will be retried.
        Errored => "ERRORED",
        /// Out of retries.
        Failed => "FAILED",
        Completed => "COMPLETED",
    }
);

impl ReconcilerState {
    /// From the pgqueue `state` and `num_failures` of a changeset: a queued row that failed
    /// before is errored (pgqueue re-queues errored jobs with a backoff).
    pub fn from_queue(state: &str, num_failures: i32) -> Self {
        match state {
            "scheduled" => Self::Scheduled,
            "queued" if num_failures > 0 => Self::Errored,
            "queued" => Self::Queued,
            "processing" => Self::Processing,
            "errored" => Self::Errored,
            "completed" => Self::Completed,
            _ => Self::Failed,
        }
    }
}

str_enum!(
    /// Whether a changeset has been created on the code host.
    PublicationState {
        Unpublished => "UNPUBLISHED",
        Published => "PUBLISHED",
    }
);

str_enum!(
    /// Publication chosen in the UI for a spec that leaves `published` unset.
    UiPublicationState {
        Unpublished => "UNPUBLISHED",
        Draft => "DRAFT",
        Published => "PUBLISHED",
    }
);

str_enum!(
    /// The state of a changeset on the code host.
    ExternalState {
        Draft => "DRAFT",
        Open => "OPEN",
        Closed => "CLOSED",
        Merged => "MERGED",
        /// The target repository is archived.
        ReadOnly => "READONLY",
        Deleted => "DELETED",
    }
);

str_enum!(
    /// The review state of a changeset on the code host.
    ReviewState {
        Approved => "APPROVED",
        ChangesRequested => "CHANGES_REQUESTED",
        Pending => "PENDING",
        Commented => "COMMENTED",
        Dismissed => "DISMISSED",
    }
);

str_enum!(
    /// The CI state of a changeset's head commit.
    CheckState {
        Pending => "PENDING",
        Passed => "PASSED",
        Failed => "FAILED",
        Unknown => "UNKNOWN",
    }
);

str_enum!(
    /// The state of a changeset shown in the UI (GraphQL `ChangesetState`).
    ChangesetState {
        Unpublished => "UNPUBLISHED",
        Scheduled => "SCHEDULED",
        Processing => "PROCESSING",
        Retrying => "RETRYING",
        Failed => "FAILED",
        Open => "OPEN",
        Draft => "DRAFT",
        Closed => "CLOSED",
        Merged => "MERGED",
        ReadOnly => "READONLY",
        Deleted => "DELETED",
    }
);

/// The UI state of a changeset: reconciler problems and work in
/// progress first, then unpublished, then the code host's state. A completed published
/// changeset without an external state yet shows as open. Mirrors the SQL function
/// `changeset_computed_state` behind `changesets.computed_state`.
pub fn compute_changeset_state(
    reconciler: ReconcilerState,
    publication: PublicationState,
    external: Option<ExternalState>,
) -> ChangesetState {
    match reconciler {
        ReconcilerState::Errored => ChangesetState::Retrying,
        ReconcilerState::Failed => ChangesetState::Failed,
        ReconcilerState::Scheduled => ChangesetState::Scheduled,
        ReconcilerState::Queued | ReconcilerState::Processing => ChangesetState::Processing,
        ReconcilerState::Completed => match (publication, external) {
            (PublicationState::Unpublished, _) => ChangesetState::Unpublished,
            (_, None | Some(ExternalState::Open)) => ChangesetState::Open,
            (_, Some(ExternalState::Draft)) => ChangesetState::Draft,
            (_, Some(ExternalState::Closed)) => ChangesetState::Closed,
            (_, Some(ExternalState::Merged)) => ChangesetState::Merged,
            (_, Some(ExternalState::ReadOnly)) => ChangesetState::ReadOnly,
            (_, Some(ExternalState::Deleted)) => ChangesetState::Deleted,
        },
    }
}

/// A `%…%` ILIKE pattern for a search term, with wildcards escaped; `None` when blank.
pub(crate) fn like_pattern(q: &str) -> Option<String> {
    let q = q.trim();
    (!q.is_empty()).then(|| {
        format!(
            "%{}%",
            q.replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_")
        )
    })
}

/// Splits a changeset search into terms: whitespace-separated words or `"quoted phrases"`, a
/// leading `-` negates. Returns (positive, negated) ILIKE patterns.
pub(crate) fn search_terms(q: &str) -> (Vec<String>, Vec<String>) {
    let mut pos = Vec::new();
    let mut neg = Vec::new();
    let mut chars = q.chars().peekable();
    loop {
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        let Some(&first) = chars.peek() else { break };
        let negated = first == '-';
        if negated {
            chars.next();
        }
        let mut term = String::new();
        if chars.peek() == Some(&'"') {
            chars.next();
            for c in chars.by_ref() {
                if c == '"' {
                    break;
                }
                term.push(c);
            }
        } else {
            while let Some(&c) = chars.peek() {
                if c.is_whitespace() {
                    break;
                }
                term.push(c);
                chars.next();
            }
        }
        if let Some(p) = like_pattern(&term) {
            if negated { &mut neg } else { &mut pos }.push(p);
        }
    }
    (pos, neg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn computed_state() {
        use ChangesetState as S;
        use ExternalState as E;
        use PublicationState as P;
        use ReconcilerState as R;
        assert_eq!(
            compute_changeset_state(R::Errored, P::Published, Some(E::Open)),
            S::Retrying
        );
        assert_eq!(
            compute_changeset_state(R::Failed, P::Unpublished, None),
            S::Failed
        );
        assert_eq!(
            compute_changeset_state(R::Scheduled, P::Unpublished, None),
            S::Scheduled
        );
        assert_eq!(
            compute_changeset_state(R::Queued, P::Published, Some(E::Merged)),
            S::Processing
        );
        assert_eq!(
            compute_changeset_state(R::Completed, P::Unpublished, None),
            S::Unpublished
        );
        assert_eq!(
            compute_changeset_state(R::Completed, P::Published, None),
            S::Open
        );
        assert_eq!(
            compute_changeset_state(R::Completed, P::Published, Some(E::Draft)),
            S::Draft
        );
        assert_eq!(
            compute_changeset_state(R::Completed, P::Published, Some(E::ReadOnly)),
            S::ReadOnly
        );
        assert_eq!(ReconcilerState::from_queue("queued", 0), R::Queued);
        assert_eq!(ReconcilerState::from_queue("queued", 2), R::Errored);
        assert_eq!(ReconcilerState::from_queue("canceled", 0), R::Failed);
    }

    #[test]
    fn search_term_parsing() {
        let (pos, neg) = search_terms(r#"fix "hello world" -wip 50%"#);
        assert_eq!(pos, vec!["%fix%", "%hello world%", "%50\\%%"]);
        assert_eq!(neg, vec!["%wip%"]);
        assert_eq!(search_terms("   "), (vec![], vec![]));
    }
}
