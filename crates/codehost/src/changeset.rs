//! Merge requests for bulk changes: [`ChangesetSource`].
//!
//! The reconciler drives changesets through this trait and stays host-neutral: it maps
//! [`Changeset::state`] to `changesets.external_state`, [`Changeset::review_state`] and
//! [`Changeset::check_state`] to the review and check columns, and stores
//! [`Changeset::events`] in `changeset_events` (keyed by `(changeset, kind, key)`, so a sync
//! upserts the same rows again) for the timeline and the burndown chart.
//! [`CodeHostError::Archived`] marks a read-only repository (external state `READONLY`).
//!
//! Changeset commits are normally pushed with git ([`ChangesetSource::push_strategy`]). A
//! source may also create them through the host's API ([`ChangesetSource::create_commit`],
//! the commit-via-API mode), so the host creates and signs them.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{CodeHostError, Credential, RepoMeta, Result};

/// A changeset to open on (or apply to) the code host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewChangeset {
    /// The title without any draft marker; the source adds the host's marker for drafts.
    pub title: String,
    pub body: String,
    /// The branch with the changes (in the target repo, or in [`head_repo_id`]'s fork).
    ///
    /// [`head_repo_id`]: Self::head_repo_id
    pub head_ref: String,
    /// The branch to merge into.
    pub base_ref: String,
    /// Open as a draft (`Draft:` or `WIP:` prefix depending on the GitLab version).
    pub draft: bool,
    /// Labels to add (labels already on the changeset are kept).
    #[serde(default)]
    pub labels: Vec<String>,
    /// Delete the head branch when the changeset is merged (`batchChanges.autoDeleteBranch`).
    #[serde(default)]
    pub remove_source_branch: bool,
    /// The external id of the repo holding `head_ref` when it is not the target repo (a fork
    /// from [`ChangesetSource::get_fork`]).
    #[serde(default)]
    pub head_repo_id: Option<String>,
}

/// The external state of a changeset (`changesets.external_state`, except `DELETED`, which
/// the reconciler sets when [`ChangesetSource::load`] answers [`CodeHostError::NotFound`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ChangesetState {
    Draft,
    Open,
    Closed,
    Merged,
    /// The repository is archived: the changeset can no longer be changed.
    #[serde(rename = "READONLY")]
    ReadOnly,
}

impl ChangesetState {
    /// The database spelling, e.g. `READONLY`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Draft => "DRAFT",
            Self::Open => "OPEN",
            Self::Closed => "CLOSED",
            Self::Merged => "MERGED",
            Self::ReadOnly => "READONLY",
        }
    }
}

/// The aggregate review state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ReviewState {
    Pending,
    Approved,
    ChangesRequested,
}

impl ReviewState {
    /// The database spelling, e.g. `CHANGES_REQUESTED`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "PENDING",
            Self::Approved => "APPROVED",
            Self::ChangesRequested => "CHANGES_REQUESTED",
        }
    }
}

/// The aggregate CI state of the changeset's head.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CheckState {
    /// There are checks, but their result is not meaningful (skipped, manual, …).
    Unknown,
    Pending,
    Passed,
    Failed,
}

impl CheckState {
    /// The database spelling, e.g. `PASSED`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "UNKNOWN",
            Self::Pending => "PENDING",
            Self::Passed => "PASSED",
            Self::Failed => "FAILED",
        }
    }
}

/// What happened in a [`ChangesetEvent`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ChangesetEventKind {
    /// A user comment.
    Commented,
    ReviewRequested,
    Approved,
    Unapproved,
    ChangesRequested,
    MarkedDraft,
    MarkedReady,
    Closed,
    Reopened,
    Merged,
    /// A CI run (pipeline); `metadata.state` holds its [`CheckState`].
    Check,
}

impl ChangesetEventKind {
    /// The database spelling, e.g. `MARKED_DRAFT`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Commented => "COMMENTED",
            Self::ReviewRequested => "REVIEW_REQUESTED",
            Self::Approved => "APPROVED",
            Self::Unapproved => "UNAPPROVED",
            Self::ChangesRequested => "CHANGES_REQUESTED",
            Self::MarkedDraft => "MARKED_DRAFT",
            Self::MarkedReady => "MARKED_READY",
            Self::Closed => "CLOSED",
            Self::Reopened => "REOPENED",
            Self::Merged => "MERGED",
            Self::Check => "CHECK",
        }
    }
}

/// One timeline event of a changeset (a `changeset_events` row).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangesetEvent {
    pub kind: ChangesetEventKind,
    /// Unique per changeset and kind and stable across syncs, e.g. `note:123`.
    pub key: String,
    pub timestamp: DateTime<Utc>,
    /// Host-specific details (actor, pipeline status, …).
    pub metadata: serde_json::Value,
}

/// A changeset as loaded from the code host.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Changeset {
    /// The number within the repo (GitLab `iid`); the changeset's external id.
    pub number: i64,
    /// The title as on the host (including any draft marker).
    pub title: String,
    pub body: String,
    pub state: ChangesetState,
    /// `None` when not loaded (write operations) or unknown.
    pub review_state: Option<ReviewState>,
    /// `None` when not loaded or there are no checks.
    pub check_state: Option<CheckState>,
    pub head_ref: String,
    pub base_ref: String,
    /// The external id of the repo holding `head_ref` when it is a fork.
    pub head_repo_id: Option<String>,
    pub labels: Vec<String>,
    pub url: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// The timeline, oldest first. Empty for the results of write operations; complete for
    /// [`ChangesetSource::load`].
    pub events: Vec<ChangesetEvent>,
    /// Host-specific metadata (`changesets.metadata`), e.g. the raw merge request.
    pub metadata: serde_json::Value,
}

/// The result of [`ChangesetSource::create`].
#[derive(Debug, Clone, PartialEq)]
pub struct CreatedChangeset {
    pub changeset: Changeset,
    /// A changeset for the head branch already existed; it was updated instead.
    pub already_existed: bool,
}

/// Options of [`ChangesetSource::merge`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MergeOptions {
    pub squash: bool,
    /// Delete the head branch after merging.
    pub remove_source_branch: bool,
}

/// Where the reconciler pushes changeset branches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushStrategy {
    /// The URL to push to (the repo or a fork), with credentials.
    pub push_url: url::Url,
    /// The fork's namespace, when pushing to a fork.
    pub fork_namespace: Option<String>,
}

/// What a [`CommitFile`] does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommitAction {
    Create,
    /// Changes the content and/or the execute flag of an existing file.
    Update,
    Delete,
    /// Renames [`CommitFile::previous_path`] to [`CommitFile::path`] (the content may change
    /// too).
    Move,
}

/// One file change of a [`NewCommit`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitFile {
    pub action: CommitAction,
    pub path: String,
    /// [`CommitAction::Move`]: the old path.
    pub previous_path: Option<String>,
    /// The new content; `None` keeps the old content (a pure move or a mode change).
    pub content: Option<Vec<u8>>,
    /// Sets (or clears) the execute flag; `None` leaves the mode as it is (new files are not
    /// executable).
    pub executable: Option<bool>,
}

/// A commit for the host to create on a branch ([`ChangesetSource::create_commit`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewCommit {
    /// The branch name (without `refs/heads/`).
    pub branch: String,
    pub message: String,
    pub author_name: String,
    pub author_email: String,
    /// The parent commit (the changeset's base).
    pub start_sha: String,
    /// The external id of the repo holding `start_sha` when it is not the repo committed to
    /// (committing to a fork).
    pub start_repo_id: Option<String>,
    /// Replace the branch when it already exists (like a force push).
    pub force: bool,
    pub files: Vec<CommitFile>,
}

/// Opens and manages changesets on a code host.
///
/// A source acts with the credential it was created with (the connection's token);
/// [`with_credential`](Self::with_credential) derives one acting with a bulk changes user or
/// site credential.
#[async_trait]
pub trait ChangesetSource: Send + Sync {
    /// A source for the same connection acting with `cred`.
    fn with_credential(&self, cred: &Credential) -> Result<Box<dyn ChangesetSource>>;
    /// Opens a changeset (as a draft when requested and supported). When one already exists
    /// for the head branch, it is updated to `new` and returned with `already_existed`.
    async fn create(&self, repo: &RepoMeta, new: &NewChangeset) -> Result<CreatedChangeset>;
    /// Marks a draft changeset ready for review.
    async fn undraft(&self, repo: &RepoMeta, number: i64) -> Result<Changeset>;
    /// Updates title, body, base branch and labels. `new.draft` makes it a draft; a changeset
    /// that already is one stays a draft (use [`undraft`](Self::undraft)).
    async fn update(&self, repo: &RepoMeta, number: i64, new: &NewChangeset) -> Result<Changeset>;
    /// Closes a changeset.
    async fn close(&self, repo: &RepoMeta, number: i64) -> Result<Changeset>;
    /// Reopens a closed changeset.
    async fn reopen(&self, repo: &RepoMeta, number: i64) -> Result<Changeset>;
    /// Merges a changeset. One the host refuses to merge fails with
    /// [`CodeHostError::NotMergeable`].
    async fn merge(&self, repo: &RepoMeta, number: i64, opts: MergeOptions) -> Result<Changeset>;
    /// Adds a comment.
    async fn create_comment(&self, repo: &RepoMeta, number: i64, body: &str) -> Result<()>;
    /// Loads the current state with review and check state and the timeline (sync).
    async fn load(&self, repo: &RepoMeta, number: i64) -> Result<Changeset>;
    /// Returns (creating it if needed, and waiting until it is ready) the fork of `repo` in
    /// `namespace` (the credential user's own when `None`).
    async fn get_fork(&self, repo: &RepoMeta, namespace: Option<&str>) -> Result<RepoMeta>;
    /// Where to push branches for `repo` (the target repo or a fork) with `cred`.
    fn push_strategy(&self, repo: &RepoMeta, cred: &Credential) -> Result<PushStrategy>;
    /// Maps a failed push's output to a typed error ([`CodeHostError::Archived`] for a
    /// read-only repo), or `None` when it is not recognized.
    fn classify_push_error(&self, _repo: &RepoMeta, _output: &str) -> Option<CodeHostError> {
        None
    }
    /// Whether [`create_commit`](Self::create_commit) is available.
    fn supports_commit_api(&self) -> bool {
        false
    }
    /// Creates `commit` on `repo` (the target repo or a fork) through the host's API instead
    /// of a git push, so the host authors and signs it. Returns the new commit's id.
    async fn create_commit(&self, _repo: &RepoMeta, _commit: &NewCommit) -> Result<String> {
        Err(CodeHostError::Unsupported(
            "creating commits through the code host API".into(),
        ))
    }
    /// Checks that `cred` can be used to open changesets (valid, with the needed scopes).
    async fn validate_credential(&self, cred: &Credential) -> Result<()>;
}
