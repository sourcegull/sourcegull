//! Batch Changes over GraphQL, with stable type and field names so
//! command-line clients keep working: the `BatchChange`, `BatchSpec`,
//! `VisibleChangesetSpec`/`HiddenChangesetSpec`, `ExternalChangeset`/`HiddenExternalChangeset`,
//! `ChangesetEvent`, `VisibleBatchSpecWorkspace`/`HiddenBatchSpecWorkspace` and
//! `BatchChangesCredential` nodes; `batchChange`, `batchChanges`, `batchSpecs`,
//! `batchChangesCodeHosts`, `maxUnlicensedChangesets` and `User`/`Org` fields; and the
//! mutations of a local run (`createChangesetSpec(s)`, `createBatchSpec`), of a server-side
//! run before execution (`createEmptyBatchChange`, `upsertEmptyBatchChange`,
//! `createBatchSpecFromRaw`, `replaceBatchSpecInput`, `deleteBatchSpec`), `closeBatchChange`,
//! `deleteBatchChange`, credentials, previews (`BatchSpec.applyPreview`) and applies
//! (`applyBatchChange`, `createBatchChange`, see [`apply`]; `sourcegull_graphql::batches::auto_apply`
//! applies specs executed with `autoApplyEnabled`), server-side execution
//! (`executeBatchSpec`, `cancelBatchSpecExecution`, `retryBatchSpecExecution`,
//! `retryBatchSpecWorkspaceExecution`, `enqueueBatchSpecWorkspaceExecution`, workspace `stages`
//! and `steps`, see [`execution`]), bulk operations (`BulkOperation`,
//! `createChangesetComments`, `reenqueueChangesets`, `mergeChangesets`, `closeChangesets`,
//! `publishChangesets`, `detachChangesets`), the burndown chart
//! (`BatchChange.changesetCountsOverTime`), a CSV export of changesets
//! (`BatchChange.changesetsCSV`), and for local runs
//! `resolveWorkspacesForBatchSpec` and the files server-side specs mount (`BatchSpec.files`,
//! see [`local`]).
//!
//! Access: everything fails with `ErrBatchChangesDisabled` when
//! `batchChanges.enabled` is off and with `ErrBatchChangesDisabledForUser` for non-admins under
//! `batchChanges.restrictToAdmins`. Reads need `BATCH_CHANGES#READ` and writes
//! `BATCH_CHANGES#WRITE` (RBAC).
//! Specs and bulk changes are created in a namespace the viewer may act for (themself, an
//! organization they belong to, or any as a site admin); a bulk change is closed or deleted by
//! its creator or a site admin (or any member of its organization with the org setting
//! `orgs.allMembersBatchChangesAdmin`).
//!
//! Repository permissions: a changeset or changeset spec in a repository the viewer may
//! not read is a `HiddenExternalChangeset`/`HiddenChangesetSpec` (id, state and dates only),
//! and a hidden workspace a `HiddenBatchSpecWorkspace`. Listings of a bulk change's
//! changesets include hidden ones (and count them) unless a filter could reveal something about
//! them (review or check state, text search, repository), in which case they are left out of
//! the nodes and the count. Stats count every changeset.

mod apply;
mod batch_change;
mod batch_spec;
mod bulk;
mod changeset;
mod credentials;
mod execution;
mod feature;
mod local;
mod preview_diff;

// `crate::context` and `crate::pagination`, as in the schema assembly.
use sourcegull_graphql_core::{context, pagination};

use std::collections::HashMap;

use async_graphql::{
    Context, Enum, ErrorExtensions, ID, InputValueError, InputValueResult, Object, Result, Scalar,
    ScalarType, Value,
};
use sourcegull_authn::{Actor, Permission};
use sourcegull_batches::store::{BatchChange, BatchChangeStore};
use sourcegull_db::{ListReposOptions, Namespace};
use sourcegull_services::relay::{marshal_id, marshal_string_id, unmarshal_id, unmarshal_int_id};
use sourcegull_types::{Repo, RepoId, UserId};

use crate::context::{
    access_error, codes, db_error, error, repo_access, require_site_admin, require_viewer, services,
};
use sourcegull_graphql_core::EmptyResponse;
use sourcegull_graphql_core::node::{NodeFuture, NodeLoader};
use sourcegull_graphql_core::shared::{NamespaceRef, NamespaceValue, UserRef, load_namespace};
use sourcegull_services::batches::access::{self, Unavailable};

pub use batch_change::list_batch_changes;
pub use batch_change::{BatchChangeConnection, BatchChangeNode, BatchChangesArgs};
pub use batch_spec::{
    BatchSpecNode, BatchSpecWorkspaceNode, ChangesetSpecNode, HiddenBatchSpecWorkspaceNode,
    HiddenChangesetSpecNode, VisibleBatchSpecWorkspaceNode, VisibleChangesetSpecNode,
};
pub use bulk::{
    BULK_OPERATION_KIND, BulkOperationConnection, BulkOperationNode, BulkOperationState,
    BulkOperationType, ChangesetJobError, bulk_operation_id,
};
pub use changeset::{
    ChangesetEventNode, ChangesetNode, ExternalChangesetNode, HiddenExternalChangesetNode,
};
pub use credentials::{BatchChangesCodeHostConnection, CredentialNode, code_hosts};
pub use feature::BatchesGraphql;
pub use local::{
    ResolvedWorkspace, WORKSPACE_FILE_KIND, WorkspaceFileConnection, WorkspaceFileNode,
};

/// Node kind of bulk change IDs.
pub const BATCH_CHANGE_KIND: &str = "BatchChange";
/// Node kind of bulk change spec IDs (keyed by the spec's random id).
pub const BATCH_SPEC_KIND: &str = "BatchSpec";
/// Node kind of changeset spec IDs (keyed by the spec's random id).
pub const CHANGESET_SPEC_KIND: &str = "ChangesetSpec";
/// Node kind of changeset IDs.
pub const CHANGESET_KIND: &str = "Changeset";
/// Node kind of changeset event IDs.
pub const CHANGESET_EVENT_KIND: &str = "ChangesetEvent";
/// Node kind of credential IDs (`"user:<id>"` or `"site:<id>"`).
pub const CREDENTIAL_KIND: &str = "BatchChangesCredential";
/// Node kind of workspace IDs.
pub const WORKSPACE_KIND: &str = "BatchSpecWorkspace";

/// Error codes (`extensions.code`).
pub mod batch_codes {
    pub const DISABLED: &str = "ErrBatchChangesDisabled";
    pub const DISABLED_FOR_USER: &str = "ErrBatchChangesDisabledForUser";
    pub const INVALID_FIRST: &str = "ErrInvalidFirstParameter";
    pub const DUPLICATE_CREDENTIAL: &str = "ErrDuplicateCredential";
    pub const VERIFY_CREDENTIAL_FAILED: &str = "ErrVerifyCredentialFailed";
    pub const VERIFY_CREDENTIAL_TIMEOUT: &str = "ErrVerifyCredentialTimeout";
    pub const INVALID_NAME: &str = "ErrBatchChangeInvalidName";
    pub const MATCHING_BATCH_CHANGE_EXISTS: &str = "ErrMatchingBatchChangeExists";
    pub const ENSURE_BATCH_CHANGE_FAILED: &str = "ErrEnsureBatchChangeFailed";
    pub const APPLY_CLOSED_BATCH_CHANGE: &str = "ErrApplyClosedBatchChange";
}

/// Largest `first` accepted.
const MAX_FIRST: i32 = 10_000;

/// Fails unless bulk changes are enabled for the signed-in viewer.
pub(crate) fn require_enabled<'a>(ctx: &Context<'a>) -> Result<&'a Actor> {
    let actor = require_viewer(ctx)?;
    let config = services(ctx)?.config.load();
    match access::available(&config.value, actor) {
        Ok(()) => Ok(actor),
        Err(Unavailable::Disabled) => Err(error(
            batch_codes::DISABLED,
            "bulk changes are disabled. Ask a site admin to set 'batchChanges.enabled' in the \
             site configuration to enable the feature.",
        )),
        Err(Unavailable::DisabledForUser) => Err(error(
            batch_codes::DISABLED_FOR_USER,
            "bulk changes are disabled for non-site-admin users. Ask a site admin to unset \
             'batchChanges.restrictToAdmins' in the site configuration to enable the feature \
             for all users.",
        )),
    }
}

fn require_permission<'a>(ctx: &Context<'a>, p: Permission) -> Result<&'a Actor> {
    let actor = require_enabled(ctx)?;
    sourcegull_authn::access::require_permission(actor, p).map_err(access_error)?;
    Ok(actor)
}

/// [`require_enabled`] plus `BATCH_CHANGES#READ`.
pub(crate) fn require_read<'a>(ctx: &Context<'a>) -> Result<&'a Actor> {
    require_permission(ctx, Permission::BatchChangesRead)
}

/// [`require_enabled`] plus `BATCH_CHANGES#WRITE`.
pub(crate) fn require_write<'a>(ctx: &Context<'a>) -> Result<&'a Actor> {
    require_permission(ctx, Permission::BatchChangesWrite)
}

fn invalid(message: impl Into<String>) -> async_graphql::Error {
    error(codes::INVALID_INPUT, message)
}

fn not_found(what: &str) -> async_graphql::Error {
    error(codes::NOT_FOUND, format!("{what} not found."))
}

/// Checks `first` (`0..=10000`, 50 when absent).
fn check_first(first: Option<i32>) -> Result<i64> {
    let first = first.unwrap_or(50);
    if (0..=MAX_FIRST).contains(&first) {
        Ok(i64::from(first))
    } else {
        Err(error(
            batch_codes::INVALID_FIRST,
            format!("first param {first} is out of range (min=0, max={MAX_FIRST})"),
        ))
    }
}

/// An `after` cursor: the id of the last row of the previous page, as a decimal string.
fn parse_cursor(after: Option<&str>) -> Result<Option<i64>> {
    match after {
        None | Some("") => Ok(None),
        Some(c) => c
            .parse()
            .map(Some)
            .map_err(|_| invalid(format!("Invalid cursor {c:?}."))),
    }
}

/// The string key of an ID of `kind` (`Kind:"key"`).
fn parse_string_id(kind: &str, id: &str) -> Option<String> {
    let (k, spec) = unmarshal_id(id)?;
    if k != kind {
        return None;
    }
    serde_json::from_str(&spec).ok()
}

fn invalid_id(id: &str) -> async_graphql::Error {
    invalid(format!("Invalid ID {id:?}."))
}

/// The ID of a bulk change.
pub fn batch_change_id(id: i64) -> ID {
    ID(marshal_id(BATCH_CHANGE_KIND, id))
}

/// Parses a bulk change ID.
pub fn parse_batch_change_id(id: &str) -> Result<i64> {
    unmarshal_int_id(BATCH_CHANGE_KIND, id)
        .filter(|n| *n > 0)
        .ok_or_else(|| invalid_id(id))
}

/// The ID of a bulk change spec.
pub fn batch_spec_id(rand_id: &str) -> ID {
    ID(marshal_string_id(BATCH_SPEC_KIND, rand_id))
}

/// The ID of a changeset spec.
pub fn changeset_spec_id(rand_id: &str) -> ID {
    ID(marshal_string_id(CHANGESET_SPEC_KIND, rand_id))
}

/// The ID of a changeset.
pub fn changeset_id(id: i64) -> ID {
    ID(marshal_id(CHANGESET_KIND, id))
}

/// A JSON value (`JSONValue`).
#[derive(Debug, Clone, PartialEq)]
pub struct JsonValue(pub serde_json::Value);

#[Scalar(name = "JSONValue")]
impl ScalarType for JsonValue {
    fn parse(value: Value) -> InputValueResult<Self> {
        value
            .into_json()
            .map(JsonValue)
            .map_err(|e| InputValueError::custom(e.to_string()))
    }

    fn to_value(&self) -> Value {
        Value::from_json(self.0.clone()).unwrap_or(Value::Null)
    }
}

/// `true`, `false` or `"draft"` (`PublishedValue`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PublishedValue(pub sourcegull_batches::store::Published);

#[Scalar(name = "PublishedValue")]
impl ScalarType for PublishedValue {
    fn parse(value: Value) -> InputValueResult<Self> {
        match &value {
            Value::Boolean(b) => Ok(Self(sourcegull_batches::store::Published::Bool(*b))),
            Value::String(s) if s == "draft" => {
                Ok(Self(sourcegull_batches::store::Published::Draft))
            }
            _ => Err(InputValueError::custom(
                "a published value is true, false or \"draft\"",
            )),
        }
    }

    fn to_value(&self) -> Value {
        match self.0 {
            sourcegull_batches::store::Published::Bool(b) => Value::Boolean(b),
            sourcegull_batches::store::Published::Draft => Value::String("draft".into()),
        }
    }
}

/// Defines a GraphQL enum mirroring a `sourcegull_db` string enum (same variant names).
macro_rules! mirror_enum {
    ($(#[$meta:meta])* $name:ident ($gql:literal) <=> $db:ty { $($v:ident => $s:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Enum, Copy, Clone, Debug, PartialEq, Eq)]
        #[graphql(name = $gql)]
        pub enum $name {
            $(#[graphql(name = $s)] $v),+
        }

        impl From<$db> for $name {
            fn from(v: $db) -> Self {
                match v {
                    $(<$db>::$v => Self::$v),+
                }
            }
        }

        impl From<$name> for $db {
            fn from(v: $name) -> Self {
                match v {
                    $($name::$v => <$db>::$v),+
                }
            }
        }
    };
}

mirror_enum!(
    /// The state of a bulk change.
    BatchChangeStateGql("BatchChangeState") <=> sourcegull_batches::store::BatchChangeState {
        Open => "OPEN",
        Closed => "CLOSED",
        Draft => "DRAFT",
    }
);

mirror_enum!(
    /// The state of a changeset shown in the UI.
    ChangesetStateGql("ChangesetState") <=> sourcegull_batches::store::ChangesetState {
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

mirror_enum!(
    /// The review state of a changeset.
    ChangesetReviewStateGql("ChangesetReviewState") <=> sourcegull_batches::store::ReviewState {
        Approved => "APPROVED",
        ChangesRequested => "CHANGES_REQUESTED",
        Pending => "PENDING",
        Commented => "COMMENTED",
        Dismissed => "DISMISSED",
    }
);

/// The CI state of a changeset (`UNKNOWN` is shown as null).
#[derive(Enum, Copy, Clone, Debug, PartialEq, Eq)]
#[graphql(name = "ChangesetCheckState")]
pub enum ChangesetCheckStateGql {
    #[graphql(name = "PENDING")]
    Pending,
    #[graphql(name = "PASSED")]
    Passed,
    #[graphql(name = "FAILED")]
    Failed,
}

impl ChangesetCheckStateGql {
    fn from_db(s: sourcegull_batches::store::CheckState) -> Option<Self> {
        match s {
            sourcegull_batches::store::CheckState::Pending => Some(Self::Pending),
            sourcegull_batches::store::CheckState::Passed => Some(Self::Passed),
            sourcegull_batches::store::CheckState::Failed => Some(Self::Failed),
            sourcegull_batches::store::CheckState::Unknown => None,
        }
    }

    fn db(self) -> sourcegull_batches::store::CheckState {
        match self {
            Self::Pending => sourcegull_batches::store::CheckState::Pending,
            Self::Passed => sourcegull_batches::store::CheckState::Passed,
            Self::Failed => sourcegull_batches::store::CheckState::Failed,
        }
    }
}

/// The signed-in viewer's live user, if any.
async fn user_node(ctx: &Context<'_>, id: Option<UserId>) -> Result<Option<UserRef>> {
    let Some(id) = id else {
        return Ok(None);
    };
    Ok(services(ctx)?
        .db
        .users()
        .get_by_id(id)
        .await
        .map_err(db_error)?
        .map(UserRef))
}

/// The namespace node; an error once the namespace is gone.
async fn namespace_node(ctx: &Context<'_>, ns: Namespace) -> Result<NamespaceRef> {
    load_namespace(ctx, ns)
        .await?
        .map(NamespaceRef)
        .ok_or_else(|| error(codes::NOT_FOUND, "The namespace has been deleted."))
}

/// The page of a namespace (`/users/<name>` or `/organizations/<name>`).
async fn namespace_url(ctx: &Context<'_>, ns: Namespace) -> Result<String> {
    Ok(match namespace_node(ctx, ns).await?.0 {
        NamespaceValue::User(u) => format!("/users/{}", u.username),
        NamespaceValue::Org(o) => format!("/organizations/{}", o.name),
    })
}

/// Whether the viewer may administer bulk changes and specs of `ns`
/// ([`access::can_administer`]).
async fn can_administer(ctx: &Context<'_>, ns: Namespace) -> Result<bool> {
    access::can_administer(&services(ctx)?.db, crate::context::viewer(ctx), ns)
        .await
        .map_err(db_error)
}

/// Fails unless the viewer may create specs and bulk changes in `ns`
/// ([`access::require_namespace_access`]).
async fn require_namespace_access(ctx: &Context<'_>, actor: &Actor, ns: Namespace) -> Result<()> {
    access::require_namespace_access(&services(ctx)?.db, actor, ns)
        .await
        .map_err(access_error)
}

/// Fails unless the viewer may close or delete `bc` ([`access::require_can_modify`]).
async fn require_can_modify(ctx: &Context<'_>, actor: &Actor, bc: &BatchChange) -> Result<()> {
    access::require_can_modify(&services(ctx)?.db, actor, bc)
        .await
        .map_err(access_error)
}

/// The repositories among `ids` the viewer may read, by id.
async fn readable_repos(ctx: &Context<'_>, ids: &[RepoId]) -> Result<HashMap<RepoId, Repo>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let access = repo_access(ctx).await?;
    let mut unique = ids.to_vec();
    unique.sort_unstable_by_key(|r| r.get());
    unique.dedup();
    let opts = ListReposOptions {
        limit: i64::try_from(unique.len()).unwrap_or(i64::MAX),
        ids: Some(unique),
        access: Some(access),
        ..Default::default()
    };
    // The viewer's access is applied (`ListReposOptions::access`).
    #[allow(clippy::disallowed_methods)]
    let repos = services(ctx)?
        .db
        .repos()
        .list(&opts)
        .await
        .map_err(db_error)?;
    Ok(repos.into_iter().map(|r| (r.id, r)).collect())
}

/// One repository if the viewer may read it.
async fn readable_repo(ctx: &Context<'_>, id: RepoId) -> Result<Option<Repo>> {
    Ok(readable_repos(ctx, &[id]).await?.remove(&id))
}

/// Bulk changes queries.
#[derive(Default)]
pub struct BatchesQuery;

#[Object]
impl BatchesQuery {
    /// Looks up a bulk change by namespace and name.
    async fn batch_change(
        &self,
        ctx: &Context<'_>,
        namespace: ID,
        name: String,
    ) -> Result<Option<BatchChangeNode>> {
        require_read(ctx)?;
        let ns = sourcegull_graphql_core::ids::parse_namespace_id(&namespace)?;
        Ok(BatchChangeStore::new(&services(ctx)?.db)
            .get_by_name(ns, &name)
            .await
            .map_err(db_error)?
            .map(BatchChangeNode::new))
    }

    /// Bulk changes, newest first. Drafts (never applied) are listed only to their creator
    /// and site admins.
    async fn batch_changes(
        &self,
        ctx: &Context<'_>,
        #[graphql(default = 50)] first: Option<i32>,
        after: Option<String>,
        state: Option<BatchChangeStateGql>,
        states: Option<Vec<BatchChangeStateGql>>,
        viewer_can_administer: Option<bool>,
    ) -> Result<BatchChangeConnection> {
        let args = BatchChangesArgs {
            first,
            after,
            state,
            states,
            viewer_can_administer,
        };
        list_batch_changes(ctx, None, args).await
    }

    /// Every bulk change spec, newest first (site admins only).
    async fn batch_specs(
        &self,
        ctx: &Context<'_>,
        #[graphql(default = 50)] first: Option<i32>,
        after: Option<String>,
        include_locally_executed_specs: Option<bool>,
        exclude_empty_specs: Option<bool>,
    ) -> Result<batch_spec::BatchSpecConnection> {
        require_read(ctx)?;
        require_site_admin(ctx)?;
        batch_spec::list_specs(
            ctx,
            None,
            first,
            after,
            include_locally_executed_specs.unwrap_or(false),
            exclude_empty_specs.unwrap_or(false),
        )
        .await
    }

    /// The code hosts changesets can be published to, with their site credential.
    async fn batch_changes_code_hosts(
        &self,
        ctx: &Context<'_>,
        #[graphql(default = 50)] first: Option<i32>,
        after: Option<String>,
    ) -> Result<BatchChangesCodeHostConnection> {
        code_hosts(ctx, None, first, after).await
    }

    /// The most changesets a bulk change may have without a license: there is no limit.
    async fn max_unlicensed_changesets(&self) -> i32 {
        i32::MAX
    }

    /// Resolves the workspaces of a bulk change spec (YAML or JSON) as the viewer, without storing
    /// anything: where a local run (e.g. `gull bulk preview|apply`) runs the steps.
    /// Unsupported and ignored repositories are included and flagged.
    async fn resolve_workspaces_for_batch_spec(
        &self,
        ctx: &Context<'_>,
        batch_spec: String,
    ) -> Result<Vec<ResolvedWorkspace>> {
        local::resolve_workspaces(ctx, &batch_spec).await
    }
}

/// Bulk changes mutations.
#[derive(Default)]
pub struct BatchesMutation;

#[Object]
impl BatchesMutation {
    /// Uploads a changeset spec (JSON) from a local run. The repository must be readable.
    async fn create_changeset_spec(
        &self,
        ctx: &Context<'_>,
        changeset_spec: String,
    ) -> Result<ChangesetSpecNode> {
        let mut specs = batch_spec::create_changeset_specs(ctx, &[changeset_spec]).await?;
        specs.pop().ok_or_else(|| invalid("No changeset spec."))
    }

    /// Uploads several changeset specs at once (all or none).
    async fn create_changeset_specs(
        &self,
        ctx: &Context<'_>,
        changeset_specs: Vec<String>,
    ) -> Result<Vec<ChangesetSpecNode>> {
        batch_spec::create_changeset_specs(ctx, &changeset_specs).await
    }

    /// Creates a bulk change spec from a local run, with the changeset specs it uploaded.
    async fn create_batch_spec(
        &self,
        ctx: &Context<'_>,
        namespace: ID,
        batch_spec: String,
        changeset_specs: Vec<ID>,
    ) -> Result<BatchSpecNode> {
        batch_spec::create_batch_spec(ctx, &namespace, &batch_spec, &changeset_specs).await
    }

    /// Creates a draft bulk change with an empty spec (for server-side execution).
    async fn create_empty_batch_change(
        &self,
        ctx: &Context<'_>,
        namespace: ID,
        name: String,
    ) -> Result<BatchChangeNode> {
        batch_change::create_empty(ctx, &namespace, &name, false).await
    }

    /// Like `createEmptyBatchChange`, but returns the existing bulk change of that name.
    async fn upsert_empty_batch_change(
        &self,
        ctx: &Context<'_>,
        namespace: ID,
        name: String,
    ) -> Result<BatchChangeNode> {
        batch_change::create_empty(ctx, &namespace, &name, true).await
    }

    /// Creates a bulk change spec from raw YAML for server-side execution of `batchChange` and
    /// queues the resolution of its workspaces. Errors carry the line and column.
    #[allow(clippy::too_many_arguments)]
    async fn create_batch_spec_from_raw(
        &self,
        ctx: &Context<'_>,
        batch_spec: String,
        #[graphql(default_with = "Some(false)")] allow_ignored: Option<bool>,
        #[graphql(default_with = "Some(false)")] allow_unsupported: Option<bool>,
        #[graphql(default_with = "Some(false)")] execute: Option<bool>,
        #[graphql(default_with = "Some(false)")] no_cache: Option<bool>,
        namespace: ID,
        batch_change: ID,
    ) -> Result<BatchSpecNode> {
        let flags = batch_spec::SpecFlags {
            allow_ignored: allow_ignored.unwrap_or(false),
            allow_unsupported: allow_unsupported.unwrap_or(false),
            no_cache: no_cache.unwrap_or(false),
        };
        // `execute` is ignored: the spec is resolved and waits for `executeBatchSpec`.
        let _ = execute;
        batch_spec::create_from_raw(ctx, &batch_spec, flags, &namespace, &batch_change).await
    }

    /// Replaces the input of a server-side bulk change spec before execution (same ID): its
    /// workspaces are resolved again.
    #[allow(clippy::too_many_arguments)]
    async fn replace_batch_spec_input(
        &self,
        ctx: &Context<'_>,
        previous_spec: ID,
        batch_spec: String,
        #[graphql(default_with = "Some(false)")] allow_ignored: Option<bool>,
        #[graphql(default_with = "Some(false)")] allow_unsupported: Option<bool>,
        #[graphql(default_with = "Some(false)")] execute: Option<bool>,
        #[graphql(default_with = "Some(false)")] no_cache: Option<bool>,
    ) -> Result<BatchSpecNode> {
        let flags = batch_spec::SpecFlags {
            allow_ignored: allow_ignored.unwrap_or(false),
            allow_unsupported: allow_unsupported.unwrap_or(false),
            no_cache: no_cache.unwrap_or(false),
        };
        // `execute` is ignored: the spec is resolved and waits for `executeBatchSpec`.
        let _ = execute;
        batch_spec::replace_input(ctx, &previous_spec, &batch_spec, flags).await
    }

    /// Executes a resolved server-side bulk change spec: one executor job per workspace that is
    /// neither skipped nor fully cached (those get their changeset specs from the cache).
    /// `noCache` ignores cached results; `autoApplyEnabled` applies the spec once every
    /// execution completed. The spec's creator or a site admin only.
    async fn execute_batch_spec(
        &self,
        ctx: &Context<'_>,
        batch_spec: ID,
        no_cache: Option<bool>,
        auto_apply_enabled: Option<bool>,
    ) -> Result<BatchSpecNode> {
        execution::execute(ctx, &batch_spec, no_cache, auto_apply_enabled).await
    }

    /// Cancels a queued or processing execution (running steps stop at the executor's next
    /// heartbeat).
    async fn cancel_batch_spec_execution(
        &self,
        ctx: &Context<'_>,
        batch_spec: ID,
    ) -> Result<BatchSpecNode> {
        execution::cancel(ctx, &batch_spec).await
    }

    /// Runs the failed and canceled workspace executions of a finished execution again (also
    /// the completed ones with `includeCompleted`).
    async fn retry_batch_spec_execution(
        &self,
        ctx: &Context<'_>,
        batch_spec: ID,
        #[graphql(default_with = "Some(false)")] include_completed: Option<bool>,
    ) -> Result<BatchSpecNode> {
        execution::retry(ctx, &batch_spec, include_completed.unwrap_or(false)).await
    }

    /// Runs the finished executions of the given workspaces (of one spec) again.
    async fn retry_batch_spec_workspace_execution(
        &self,
        ctx: &Context<'_>,
        batch_spec_workspaces: Vec<ID>,
    ) -> Result<EmptyResponse> {
        execution::retry_workspaces(ctx, &batch_spec_workspaces).await
    }

    /// Queues the given workspaces (of one resolved spec): new executions, or retries of
    /// finished ones.
    async fn enqueue_batch_spec_workspace_execution(
        &self,
        ctx: &Context<'_>,
        batch_spec_workspaces: Vec<ID>,
    ) -> Result<EmptyResponse> {
        execution::enqueue_workspaces(ctx, &batch_spec_workspaces).await
    }

    /// Deletes a bulk change spec that no bulk change has applied (site admins only).
    async fn delete_batch_spec(&self, ctx: &Context<'_>, batch_spec: ID) -> Result<EmptyResponse> {
        batch_spec::delete_spec(ctx, &batch_spec).await
    }

    /// Applies a bulk change spec: creates the bulk change of its namespace and name, or updates
    /// it (with `ensureBatchChange`, only if it is that one). New changeset specs get
    /// changesets, matched ones are updated, and changesets no spec matches any more are
    /// closed and archived (or detached). `publicationStates` publishes specs that leave
    /// `published` unset.
    async fn apply_batch_change(
        &self,
        ctx: &Context<'_>,
        batch_spec: ID,
        ensure_batch_change: Option<ID>,
        publication_states: Option<Vec<apply::ChangesetSpecPublicationStateInput>>,
    ) -> Result<BatchChangeNode> {
        let applied = apply::apply(
            ctx,
            apply::ApplyOpts {
                batch_spec: &batch_spec,
                ensure: ensure_batch_change.as_ref().map(|id| id.as_str()),
                publication_states,
                create_only: false,
            },
        )
        .await?;
        sourcegull_graphql_core::events::log_backend(
            ctx,
            sourcegull_analytics::names::BATCH_CHANGE_APPLIED,
            serde_json::json!({}),
        );
        Ok(applied)
    }

    /// Like `applyBatchChange`, but fails if the bulk change exists.
    async fn create_batch_change(
        &self,
        ctx: &Context<'_>,
        batch_spec: ID,
        publication_states: Option<Vec<apply::ChangesetSpecPublicationStateInput>>,
    ) -> Result<BatchChangeNode> {
        let applied = apply::apply(
            ctx,
            apply::ApplyOpts {
                batch_spec: &batch_spec,
                ensure: None,
                publication_states,
                create_only: true,
            },
        )
        .await?;
        sourcegull_graphql_core::events::log_backend(
            ctx,
            sourcegull_analytics::names::BATCH_CHANGE_APPLIED,
            serde_json::json!({}),
        );
        Ok(applied)
    }

    /// Closes a bulk change; with `closeChangesets`, its open changesets are closed on the
    /// code host too.
    async fn close_batch_change(
        &self,
        ctx: &Context<'_>,
        batch_change: ID,
        #[graphql(default_with = "Some(false)")] close_changesets: Option<bool>,
    ) -> Result<BatchChangeNode> {
        batch_change::close(ctx, &batch_change, close_changesets.unwrap_or(false)).await
    }

    /// Deletes a bulk change. Its changesets stay on the code host.
    async fn delete_batch_change(
        &self,
        ctx: &Context<'_>,
        batch_change: ID,
    ) -> Result<Option<EmptyResponse>> {
        batch_change::delete(ctx, &batch_change).await?;
        Ok(Some(EmptyResponse))
    }

    /// Stores a code host token for publishing changesets, after checking it with the code
    /// host: the user's (the user themself or site admins), or the site's without `user`
    /// (site admins only).
    async fn create_batch_changes_credential(
        &self,
        ctx: &Context<'_>,
        user: Option<ID>,
        external_service_kind: sourcegull_graphql_core::codehost::ExternalServiceKindGql,
        #[graphql(name = "externalServiceURL")] external_service_url: String,
        username: Option<String>,
        credential: String,
    ) -> Result<CredentialNode> {
        let _ = username;
        credentials::create(
            ctx,
            user.as_ref().map(|u| u.as_str()),
            external_service_kind,
            &external_service_url,
            &credential,
        )
        .await
    }

    /// Comments on changesets of a bulk change (published ones, archived ones included).
    async fn create_changeset_comments(
        &self,
        ctx: &Context<'_>,
        batch_change: ID,
        changesets: Vec<ID>,
        body: String,
    ) -> Result<BulkOperationNode> {
        if body.trim().is_empty() {
            return Err(invalid("empty comment body is not allowed"));
        }
        let payload = serde_json::json!({ "message": body });
        let kind = sourcegull_batches::store::ChangesetJobType::Comment;
        bulk::create(ctx, &batch_change, &changesets, kind, payload).await
    }

    /// Queues failed changesets of a bulk change for the reconciler again.
    async fn reenqueue_changesets(
        &self,
        ctx: &Context<'_>,
        batch_change: ID,
        changesets: Vec<ID>,
    ) -> Result<BulkOperationNode> {
        let kind = sourcegull_batches::store::ChangesetJobType::Reenqueue;
        bulk::create(ctx, &batch_change, &changesets, kind, serde_json::json!({})).await
    }

    /// Merges open changesets of a bulk change on the code host (squashed with `squash`).
    async fn merge_changesets(
        &self,
        ctx: &Context<'_>,
        batch_change: ID,
        changesets: Vec<ID>,
        #[graphql(default_with = "Some(false)")] squash: Option<bool>,
    ) -> Result<BulkOperationNode> {
        let payload = serde_json::json!({ "squash": squash.unwrap_or(false) });
        let kind = sourcegull_batches::store::ChangesetJobType::Merge;
        bulk::create(ctx, &batch_change, &changesets, kind, payload).await
    }

    /// Closes open and draft changesets of a bulk change on the code host.
    async fn close_changesets(
        &self,
        ctx: &Context<'_>,
        batch_change: ID,
        changesets: Vec<ID>,
    ) -> Result<BulkOperationNode> {
        let kind = sourcegull_batches::store::ChangesetJobType::Close;
        bulk::create(ctx, &batch_change, &changesets, kind, serde_json::json!({})).await
    }

    /// Publishes changesets of a bulk change (as drafts with `draft`) whose spec leaves
    /// `published` unset.
    async fn publish_changesets(
        &self,
        ctx: &Context<'_>,
        batch_change: ID,
        changesets: Vec<ID>,
        #[graphql(default_with = "Some(false)")] draft: Option<bool>,
    ) -> Result<BulkOperationNode> {
        let payload = serde_json::json!({ "draft": draft.unwrap_or(false) });
        let kind = sourcegull_batches::store::ChangesetJobType::Publish;
        bulk::create(ctx, &batch_change, &changesets, kind, payload).await
    }

    /// Detaches archived changesets from a bulk change.
    async fn detach_changesets(
        &self,
        ctx: &Context<'_>,
        batch_change: ID,
        changesets: Vec<ID>,
    ) -> Result<BulkOperationNode> {
        let kind = sourcegull_batches::store::ChangesetJobType::Detach;
        bulk::create(ctx, &batch_change, &changesets, kind, serde_json::json!({})).await
    }

    /// Deletes a credential (as creating it).
    async fn delete_batch_changes_credential(
        &self,
        ctx: &Context<'_>,
        batch_changes_credential: ID,
    ) -> Result<EmptyResponse> {
        credentials::delete(ctx, &batch_changes_credential).await
    }
}

/// A bulk changes node `node(id:)` returns.
#[allow(clippy::large_enum_variant)]
pub enum BatchesNode {
    BatchChange(BatchChangeNode),
    BatchSpec(BatchSpecNode),
    ChangesetSpec(ChangesetSpecNode),
    Changeset(ChangesetNode),
    Workspace(BatchSpecWorkspaceNode),
    Credential(CredentialNode),
    BulkOperation(BulkOperationNode),
}

/// The `node(id:)` loaders of the bulk changes kinds, for the schema's node type `N`.
pub fn node_loaders<N: From<BatchesNode> + Send + 'static>() -> [NodeLoader<N>; 1] {
    fn load<'a, N: From<BatchesNode> + Send + 'static>(
        ctx: &'a Context<'_>,
        kind: &'a str,
        id: &'a str,
    ) -> NodeFuture<'a, N> {
        Box::pin(async move { Ok(load_node(ctx, kind, id).await?.map(N::from)) })
    }
    [NodeLoader {
        kinds: &[
            BATCH_CHANGE_KIND,
            BATCH_SPEC_KIND,
            CHANGESET_SPEC_KIND,
            CHANGESET_KIND,
            WORKSPACE_KIND,
            bulk::BULK_OPERATION_KIND,
            CREDENTIAL_KIND,
        ],
        load: load::<N>,
    }]
}

/// Looks up a bulk changes node of kind `kind`, if it exists.
pub async fn load_node(ctx: &Context<'_>, kind: &str, id: &str) -> Result<Option<BatchesNode>> {
    Ok(match kind {
        BATCH_CHANGE_KIND => {
            require_read(ctx)?;
            let Some(key) = unmarshal_int_id(kind, id) else {
                return Ok(None);
            };
            BatchChangeStore::new(&services(ctx)?.db)
                .get(key)
                .await
                .map_err(db_error)?
                .map(|b| BatchesNode::BatchChange(BatchChangeNode::new(b)))
        }
        BATCH_SPEC_KIND => {
            require_read(ctx)?;
            batch_spec::load_spec(ctx, id)
                .await?
                .map(BatchesNode::BatchSpec)
        }
        CHANGESET_SPEC_KIND => {
            require_read(ctx)?;
            batch_spec::load_changeset_spec(ctx, id)
                .await?
                .map(BatchesNode::ChangesetSpec)
        }
        CHANGESET_KIND => {
            require_read(ctx)?;
            changeset::load_node(ctx, id)
                .await?
                .map(BatchesNode::Changeset)
        }
        WORKSPACE_KIND => {
            require_read(ctx)?;
            batch_spec::load_workspace(ctx, id)
                .await?
                .map(BatchesNode::Workspace)
        }
        bulk::BULK_OPERATION_KIND => {
            require_read(ctx)?;
            bulk::load_node(ctx, id)
                .await?
                .map(BatchesNode::BulkOperation)
        }
        CREDENTIAL_KIND => credentials::load_node(ctx, id)
            .await?
            .map(BatchesNode::Credential),
        _ => None,
    })
}

/// Records a bulk changes security event.
async fn audit(ctx: &Context<'_>, name: &str, actor: &Actor, argument: serde_json::Value) {
    if let Ok(services) = services(ctx) {
        sourcegull_graphql_core::audit::record(services, name, Some(actor.user_id), argument).await;
    }
}

/// Maps a spec parse failure to a `VALIDATION` error listing each issue with its position
/// (`line L, column C: field: message`).
fn spec_errors(e: &sourcegull_batches::SpecErrors) -> async_graphql::Error {
    let messages: Vec<String> = e.issues().iter().map(ToString::to_string).collect();
    let issues: Vec<serde_json::Value> = e
        .issues()
        .iter()
        .map(|i| {
            serde_json::json!({
                "message": i.message,
                "path": i.path,
                "line": i.location.map(|l| l.line),
                "column": i.location.map(|l| l.column),
            })
        })
        .collect();
    crate::context::validation(messages).extend_with(move |_, ext| {
        if let Ok(v) = Value::from_json(serde_json::Value::Array(issues.clone())) {
            ext.set("issues", v);
        }
    })
}
