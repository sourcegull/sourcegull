//! Per-domain stores.

mod access_requests;
mod access_tokens;
mod accounts;
mod admin_logs;
mod declared;
mod encryption_keys;
mod event_logs;
mod executor_secrets;
mod executors;
mod external_accounts;
mod external_services;
mod feature_flags;
mod gitserver_repos;
mod oauth;
mod oobmigrations;
mod orgs;
mod password_resets;
mod perms;
mod perms_sync_jobs;
mod rbac;
mod repo_events;
mod repo_moves;
mod repo_update_jobs;
mod repos;
mod routines;
mod scim;
mod search;
mod search_index;
mod security_events;
mod sessions;
mod settings;
mod site_config;
mod sync_jobs;
mod temporary_settings;
mod user_emails;
mod users;

pub use access_requests::{
    AccessRequest, AccessRequestStatus, AccessRequestStore, INVITE_TTL_HOURS,
};
pub use access_tokens::{AccessToken, AccessTokenStore, NewAccessToken, TokenRecord};
pub use accounts::{AccountStore, CreatedAccount, NewAccount};
pub use admin_logs::{
    NewOutboundRequest, NewSlowRequest, OutboundRequestLogItem, OutboundRequestStore,
    SLOW_REQUESTS_KEPT, SlowRequestKind, SlowRequestLogItem, SlowRequestStore,
};
pub use declared::{
    DeclaredObject, DeclaredSource, DeclaredStore, GLOBAL_SETTINGS_SOURCE, Ownership,
    RESOURCES_SOURCE, ROLE_GRANT, SITE_ADMIN_GRANT,
};
pub use encryption_keys::EncryptionKeyStore;
pub use event_logs::{EventLogStore, NewUsageEvent};
pub use executor_secrets::{
    ExecutorSecret, ExecutorSecretAccessLog, ExecutorSecretScope, ExecutorSecretStore,
    ListExecutorSecrets, SecretAccessor,
};
pub use executors::{ActiveJob, Executor, ExecutorInfo, ExecutorStore, JobTokenScope};
pub use external_accounts::{
    AccountSpec, ExternalAccount, ExternalAccountStore, ExternalAccountSummary, LinkAccount,
    OAuthFlow,
};
pub use external_services::{
    ExternalServiceStore, ExternalServiceUpdate, LockedConfig, NewExternalService, SyncOutcome,
};
pub use feature_flags::{
    FeatureFlagStore, FeatureFlagValue, OverrideNamespace, StoredFeatureFlag, StoredFlagOverride,
};
pub use gitserver_repos::{FetchResult, FetchSchedule, GitserverRepoStore};
pub use oauth::{
    NewOAuthAuthorization, NewOAuthClient, NewOAuthToken, OAuthAccessRecord, OAuthAuthorization,
    OAuthClient, OAuthClientKind, OAuthGrant, OAuthStore, RefreshOutcome,
};
pub use oobmigrations::{
    MAX_MIGRATION_ERRORS, MigrationDescriptor, MinorVersion, OutOfBandMigration,
    OutOfBandMigrationError, OutOfBandMigrationStore,
};
pub use orgs::{NewOrgInvitation, OrgInvitation, OrgStore};
pub use password_resets::PasswordResetStore;
pub use perms::{BindKind, PermsChange, PermsSource, PermsStore, RepoPermsInfo, authz_payload};
pub use perms_sync_jobs::{
    ListPermsSyncJobs, PERMS_SYNC_JOBS_TABLE, PermsSyncJob, PermsSyncJobRecord, PermsSyncJobStore,
    PermsSyncReason, PermsSyncResult, PermsSyncSubject,
};
pub use rbac::{RbacStore, Role, RolePermission};
pub use repo_events::{
    NewWebhookLog, RefEvent, RefEventSource, RefEventStore, RefUpdate, WebhookLog,
    WebhookLogFilter, WebhookLogStore,
};
pub use repo_moves::{
    MoveCounts, MoveKind, NewRepoMove, OpenRepoMove, Planning, REPO_NODE_LIVE_WINDOW, RepoMove,
    RepoMoveStore, RepoNodeHeartbeat, live_repo_nodes, move_state, unsettled_repo_nodes,
};
pub use repo_update_jobs::{
    REPO_UPDATE_JOBS_TABLE, RepoUpdateJob, RepoUpdateJobRecord, RepoUpdateJobStore,
    RepoUpdatePriority, RepoUpdateReason,
};
pub use repos::{
    CodeHostRepoHints, ListReposOptions, ListedRepo, RepoOrder, RepoStore, UpsertSummary,
};
pub use routines::{
    MemoryRoutineRecorder, PgRoutineRecorder, RECENT_RUNS_KEPT, RoutineInstance, RoutineRecorder,
    RoutineRun, RoutineSnapshot, RoutineStats, STATS_DAYS, global_recorder, record_run,
    routine_host_name, set_global_recorder,
};
pub use scim::{
    SCIM_SERVICE, SCIM_USER_NAME_CONSTRAINT, ScimAccount, ScimNarrow, ScimStore, ScimUser,
    ScimWrite, ScimWritten,
};
pub use search::{
    ContextAccess, ContextOrder, ListContextsOptions, ListSavedSearchesOptions, Namespace,
    SavedSearch, SavedSearchOrder, SavedSearchStore, SavedSearchVisibility, SearchContext,
    SearchContextInput, SearchContextRepo, SearchContextStore,
};
pub use search_index::{
    CLONES_PAUSED_ERROR, IndexedBranchRow, IndexedState, NodeUsage, RepositoryStats,
    SearchIndexRepo, SearchIndexStore,
};
pub use security_events::{NewSecurityEvent, SecurityEvent, SecurityEventStore};
pub use sessions::{SessionRow, SessionRowStore};
pub use settings::{Settings, SettingsStore};
pub use site_config::{SiteConfig, SiteConfigStore};
pub use sync_jobs::{SYNC_JOBS_TABLE, SyncJob, SyncJobRecord, SyncJobStore, SyncReason, SyncStats};
pub use temporary_settings::TemporarySettingsStore;
pub use user_emails::UserEmailStore;
pub use users::{ListUsersOptions, LockoutPolicy, LoginFailure, NewUser, ProfileUpdate, UserStore};

/// Names of the seeded system roles (`roles.name`).
pub mod system_roles {
    /// Assigned to every user.
    pub const USER: &str = "USER";
    /// Assigned to site admins.
    pub const SITE_ADMINISTRATOR: &str = "SITE_ADMINISTRATOR";
}
