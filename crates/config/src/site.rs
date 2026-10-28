//! Typed site configuration.
//!
//! Keys use a flat, dotted naming (`"auth.providers"`, `"search.limits"`, `"externalURL"`).
//! Every field has a default, so `{}` is a valid site config.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sourcegull_types::AlwaysIndex;

use crate::duration::ConfigDuration;

/// The site configuration document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, rename_all = "camelCase")]
#[schemars(title = "Site configuration")]
pub struct SiteConfig {
    /// The externally reachable URL of this instance, e.g. `https://sourcegull.example.com`.
    /// Used in links, emails and OAuth callbacks.
    #[serde(rename = "externalURL", skip_serializing_if = "Option::is_none")]
    pub external_url: Option<String>,

    /// Enabled authentication providers. Several may be enabled at once.
    #[serde(rename = "auth.providers")]
    pub auth_providers: Vec<AuthProvider>,

    /// How long a session stays valid after sign-in.
    #[serde(rename = "auth.sessionExpiry")]
    pub auth_session_expiry: ConfigDuration,

    /// Lifetime of a single-use password reset link, in seconds.
    #[serde(rename = "auth.passwordResetLinkExpiry")]
    pub auth_password_reset_link_expiry: u32,

    /// Client IP restrictions for the public and internal HTTP listeners.
    #[serde(rename = "auth.allowedIpAddress")]
    pub auth_allowed_ip_address: AllowedIpAddress,

    /// SMTP delivery settings. An absent value disables outbound email.
    #[serde(rename = "email.smtp", skip_serializing_if = "Option::is_none")]
    pub email_smtp: Option<SmtpConfig>,
    /// Sender mailbox.
    #[serde(rename = "email.address", skip_serializing_if = "Option::is_none")]
    pub email_address: Option<String>,
    /// Sender display name.
    #[serde(rename = "email.senderName", skip_serializing_if = "Option::is_none")]
    pub email_sender_name: Option<String>,

    /// Rules for built-in passwords.
    #[serde(rename = "auth.passwordPolicy")]
    pub auth_password_policy: PasswordPolicy,

    /// Account lockout after repeated failed sign-in attempts.
    #[serde(rename = "auth.lockout")]
    pub auth_lockout: Lockout,

    /// Request limits per client address and per account on the endpoints anyone can call.
    #[serde(rename = "auth.rateLimits")]
    pub auth_rate_limits: AuthRateLimits,

    /// Access request form. Defaults to enabled when built-in sign-up is closed.
    #[serde(rename = "auth.accessRequest")]
    pub auth_access_request: AccessRequest,

    /// Access token policy.
    #[serde(rename = "auth.accessTokens")]
    pub auth_access_tokens: AccessTokens,

    /// Allow anonymous (signed-out) users to use the instance.
    #[serde(rename = "auth.public")]
    pub auth_public: bool,

    /// Let users change their own username in their profile settings. Site admins can always
    /// change usernames; usernames of SCIM-managed users change only through SCIM.
    #[serde(rename = "auth.enableUsernameChanges")]
    pub auth_enable_username_changes: bool,

    /// Let MCP clients sign users in through the browser (OAuth 2.1 authorization server for
    /// `/.api/mcp`). Off: MCP clients authenticate with access tokens only.
    #[serde(rename = "mcp.oauth.enabled")]
    pub mcp_oauth_enabled: bool,

    /// Let MCP clients register themselves (OAuth Dynamic Client Registration at
    /// `/.oauth/register`). Clients that identify with a client ID metadata document (an
    /// `https` URL as `client_id`) work either way.
    #[serde(rename = "mcp.oauth.dynamicClientRegistration")]
    pub mcp_oauth_dynamic_client_registration: bool,

    /// Lifetime of the access tokens MCP clients get through OAuth.
    #[serde(rename = "mcp.oauth.accessTokenTTL")]
    pub mcp_oauth_access_token_ttl: ConfigDuration,

    /// Lifetime of OAuth refresh tokens. Each refresh issues a new one; a grant not refreshed
    /// within this time ends.
    #[serde(rename = "mcp.oauth.refreshTokenTTL")]
    pub mcp_oauth_refresh_token_ttl: ConfigDuration,

    /// Hosts that `https` redirect URIs of MCP clients may point to, e.g. `["vscode.dev"]` or
    /// `["*.example.com"]`. Empty (the default): any host. Loopback redirect URIs
    /// (`http://127.0.0.1:<port>/…`, `http://localhost:<port>/…`) are always allowed.
    #[serde(
        rename = "mcp.oauth.allowedRedirectHosts",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub mcp_oauth_allowed_redirect_hosts: Vec<String>,

    /// Explicit repository permissions set through the API. When enabled, private
    /// repositories are restricted even on connections without `authorization`, and admins
    /// grant access with `setRepositoryPermissionsForUsers`.
    #[serde(rename = "permissions.userMapping")]
    pub permissions_user_mapping: PermissionsUserMapping,

    /// Apply repository permissions to site admins too. By default site admins can read every
    /// repository.
    #[serde(rename = "authz.enforceForSiteAdmins")]
    pub authz_enforce_for_site_admins: bool,

    /// Seconds between runs of the permissions sync scheduler, which queues syncs of the users
    /// and repositories synced longest ago.
    #[serde(rename = "permissions.syncScheduleInterval")]
    #[schemars(range(min = 1))]
    pub permissions_sync_schedule_interval: u32,

    /// Users queued per scheduler run.
    #[serde(rename = "permissions.syncOldestUsers")]
    pub permissions_sync_oldest_users: u32,

    /// Repositories queued per scheduler run (repo-centric sync, `identityProvider:
    /// username` connections only).
    #[serde(rename = "permissions.syncOldestRepos")]
    pub permissions_sync_oldest_repos: u32,

    /// A user is not re-synced by the scheduler within this many seconds of the last sync.
    #[serde(rename = "permissions.syncUsersBackoffSeconds")]
    pub permissions_sync_users_backoff_seconds: u32,

    /// A repository is not re-synced by the scheduler within this many seconds of the last
    /// sync.
    #[serde(rename = "permissions.syncReposBackoffSeconds")]
    pub permissions_sync_repos_backoff_seconds: u32,

    /// Finished permissions sync jobs kept per user and per repository.
    #[serde(rename = "permissions.syncJobsHistorySize")]
    #[schemars(range(min = 1))]
    pub permissions_sync_jobs_history_size: u32,

    /// Logging settings.
    pub log: LogConfig,

    /// How long raw usage events (`event_logs`) are kept, e.g. `"93d"`. The worker
    /// rolls them up into daily usage analytics first, then deletes older events. Usage events
    /// never leave the instance.
    #[serde(rename = "eventLogs.retention")]
    pub event_logs_retention: ConfigDuration,

    /// How long the daily usage analytics rollups (site admin > Analytics) are kept, e.g.
    /// `"730d"`. At least as long as `eventLogs.retention` is used.
    #[serde(rename = "eventLogs.rollupRetention")]
    pub event_logs_rollup_retention: ConfigDuration,

    /// Log GraphQL requests slower than this many milliseconds: a structured log line
    /// and an entry on the site admin "Slow requests" page. Unset: off.
    #[serde(
        rename = "observability.logSlowGraphQLRequests",
        skip_serializing_if = "Option::is_none"
    )]
    pub observability_log_slow_graphql_requests: Option<u32>,

    /// Log searches slower than this many milliseconds: a structured log line and an
    /// entry on the site admin "Slow requests" page. Unset: off.
    #[serde(
        rename = "observability.logSlowSearches",
        skip_serializing_if = "Option::is_none"
    )]
    pub observability_log_slow_searches: Option<u32>,

    /// How many outbound code host API requests the outbound request log keeps, across
    /// all processes; the oldest are dropped. 0 (the default) logs none.
    #[serde(rename = "outboundRequestLogLimit")]
    pub outbound_request_log_limit: u32,

    /// Search limits and timeouts.
    #[serde(rename = "search.limits")]
    pub search_limits: SearchLimits,

    /// Branches to index besides the default branch, per repository name, e.g.
    /// `{"gitlab.com/acme/api": ["release", "dev"]}`. Branches the index does not
    /// hold are searched unindexed. At most 63 per repository.
    #[serde(
        rename = "search.index.branches",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub search_index_branches: BTreeMap<String, Vec<String>>,

    /// Path patterns whose files are indexed with content even above the 1 MiB size
    /// limit, up to 16 MiB, per repository name; `"*"` applies to every repository, e.g.
    /// `{"*": ["**/schema.graphql"], "gitlab.com/acme/api": ["gen/**/*.ts"]}`. Patterns
    /// match the whole path: `*` stays within a directory, `**` spans any number of them.
    /// Unindexed search follows the same rule.
    #[serde(
        rename = "search.index.alwaysIndex",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub search_index_always_index: BTreeMap<String, Vec<String>>,

    /// Minutes between discovery syncs of each code host connection. Syncs between
    /// full listings are incremental, so a short interval is cheap.
    #[serde(rename = "repoListUpdateInterval")]
    #[schemars(range(min = 1))]
    pub repo_list_update_interval: u32,

    /// Turn off periodic background fetches of mirrored repositories.
    /// Clones of new repos, webhook-triggered fetches and "update now" still run.
    #[serde(rename = "disableAutoGitUpdates")]
    pub disable_auto_git_updates: bool,

    /// Fixed fetch intervals for repos whose name matches a pattern, instead of the adaptive
    /// schedule. The first matching rule wins.
    #[serde(rename = "gitUpdateInterval", skip_serializing_if = "Vec::is_empty")]
    pub git_update_interval: Vec<GitUpdateIntervalRule>,

    /// Concurrent clones per repo node. Read when the repo node starts.
    #[serde(rename = "gitMaxConcurrentClones")]
    #[schemars(range(min = 1))]
    pub git_max_concurrent_clones: u32,

    /// Repositories pinned to repo nodes: repository name → repo node address, e.g.
    /// `{"gitlab.com/acme/big": "http://sourcegull-repo-2.sourcegull-repo:3178"}`. The rebalancer
    /// moves a pinned repository to its node, and back to its hash assignment when unpinned.
    #[serde(
        rename = "repoNodes.pinnedRepos",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub repo_nodes_pinned_repos: BTreeMap<String, String>,

    /// The shared secret executors authenticate with, at least 20 characters. Without
    /// it the executor queue API is disabled.
    #[serde(
        rename = "executors.accessToken",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(length(min = 20))]
    pub executors_access_token: Option<String>,

    /// The bearer token identity providers send to the SCIM 2.0 API (`/.api/scim/v2`),
    /// at least 20 characters. Without it SCIM provisioning is disabled.
    #[serde(rename = "scim.authToken", skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 20))]
    pub scim_auth_token: Option<String>,

    /// Accept SCIP uploads authenticated by a GitLab CI job token (`CI_JOB_TOKEN`, sent by
    /// `gull code-intel upload` as `Authorization: token-gitlab-ci …`) for the repository of the
    /// job's GitLab project, checked with the code host's `GET /api/v4/job`.
    #[serde(rename = "codeIntel.gitlabJobTokenUploads")]
    pub code_intel_gitlab_job_token_uploads: bool,
    /// The largest SCIP upload accepted, in bytes as sent (usually gzip-compressed), summed
    /// over the parts of a multipart upload. Larger uploads are answered 413 and discarded.
    /// Default 2 GiB.
    #[serde(rename = "codeIntel.maxUploadBytes")]
    #[schemars(range(min = 1))]
    pub code_intel_max_upload_bytes: u64,
    /// Rank files by PageRank over the code graph of SCIP uploads and use the ranks
    /// to order search results. On by default; it does nothing until uploads exist.
    /// Turning it off drops the ranks from the search index at the next reconcile.
    #[serde(rename = "codeIntelRanking.documentReferenceCountsEnabled")]
    pub codeintel_ranking_enabled: bool,

    /// Auto-indexing: infer index jobs for commits that configuration policies
    /// with indexing enabled name, and queue them for executors.
    #[serde(rename = "codeIntelAutoIndexing.enabled")]
    pub code_intel_auto_indexing_enabled: bool,

    /// Let global configuration policies (those without repositories) enable auto-indexing
    /// for every repository. Off by default.
    #[serde(rename = "codeIntelAutoIndexing.allowGlobalPolicies")]
    pub code_intel_auto_indexing_allow_global_policies: bool,

    /// The most repositories one policy with repository patterns schedules; -1 for no limit.
    #[serde(rename = "codeIntelAutoIndexing.policyRepositoryMatchLimit")]
    pub code_intel_auto_indexing_policy_repository_match_limit: i32,

    /// Indexer images by language key (`go`, `java`, `python`, `typescript`, `ruby`, `rust`,
    /// `dotnet`, `clang`), replacing the pinned defaults, e.g. for a registry mirror.
    #[serde(
        rename = "codeIntelAutoIndexing.indexerMap",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub code_intel_auto_indexing_indexer_map: BTreeMap<String, String>,

    /// The most commits per repository and hour that ref updates queue for inference (the
    /// periodic sweep picks up the rest).
    #[serde(rename = "codeIntelAutoIndexing.repositoryCommitsPerHour")]
    #[schemars(range(min = 1))]
    pub code_intel_auto_indexing_repository_commits_per_hour: u32,

    /// Push bulk change branches to a fork of the repository (in the credential user's
    /// namespace, or the changeset template's `fork` namespace) instead of the repository
    /// itself.
    #[serde(rename = "batchChanges.enforceForks")]
    pub batch_changes_enforce_forks: bool,

    /// Ask the code host to delete a changeset's branch when its merge request is merged
    /// (GitLab `remove_source_branch`).
    #[serde(rename = "batchChanges.autoDeleteBranch")]
    pub batch_changes_auto_delete_branch: bool,

    /// Changesets the reconciler processes at once in each worker process. Read when
    /// the worker starts.
    #[serde(rename = "batchChanges.reconcilerConcurrency")]
    #[schemars(range(min = 1))]
    pub batch_changes_reconciler_concurrency: u32,

    /// Batch Changes. On by default; when off, every batch
    /// changes API fails with `ErrBatchChangesDisabled`.
    #[serde(rename = "batchChanges.enabled")]
    pub batch_changes_enabled: bool,

    /// Only site admins may use Batch Changes.
    #[serde(rename = "batchChanges.restrictToAdmins")]
    pub batch_changes_restrict_to_admins: bool,

    /// Create changeset commits through the code host's API instead of `git push` from the repo
    /// nodes: on GitLab, `POST /projects/:id/repository/commits`, so GitLab creates the commit and
    /// signs it where web commit signing is set up (GitLab 15.x+; on self-managed instances Gitaly
    /// needs a signing key). Changes larger than `batchChanges.commitViaAPIMaxBytes` or
    /// `batchChanges.commitViaAPIMaxFiles`, or with symlinks or submodules, are pushed with git
    /// instead (unless `batchChanges.rejectUnverifiedCommit` is set).
    #[serde(rename = "batchChanges.commitViaAPI")]
    pub batch_changes_commit_via_api: bool,

    /// Never push unsigned changeset commits: implies `batchChanges.commitViaAPI`, and a
    /// changeset whose commit cannot be created through the code host's API (too large,
    /// symlinks or submodules, or a code host without a commit API) fails with an error
    /// instead of falling back to `git push`.
    #[serde(rename = "batchChanges.rejectUnverifiedCommit")]
    pub batch_changes_reject_unverified_commit: bool,

    /// The most bytes of changed file contents (before base64) a commit created through the
    /// API may carry; larger changes fall back to `git push`. At most 48 MiB.
    #[serde(rename = "batchChanges.commitViaAPIMaxBytes")]
    #[schemars(range(min = 1, max = 50331648))]
    pub batch_changes_commit_via_api_max_bytes: u64,

    /// The most changed files a commit created through the API may carry; larger changes
    /// fall back to `git push`.
    #[serde(rename = "batchChanges.commitViaAPIMaxFiles")]
    #[schemars(range(min = 1))]
    pub batch_changes_commit_via_api_max_files: u32,
    /// How long changesets that no bulk change uses any more are kept: archived in every
    /// bulk change tracking them, or detached from all of them, e.g. `"720h"`. A
    /// worker singleton deletes them (with their events) once this has passed. Unset (the
    /// default): kept forever.
    #[serde(
        rename = "batchChanges.changesetsRetention",
        skip_serializing_if = "Option::is_none"
    )]
    pub batch_changes_changesets_retention: Option<ConfigDuration>,

    /// When and how fast changesets are published: each window
    /// has optional `days` and `start`/`end` (`hh:mm`, UTC) and a `rate` (`"10/hour"`,
    /// `"unlimited"`, `0`). Outside every window nothing is published; the last matching
    /// window wins. Empty: changesets are published right away.
    #[serde(
        rename = "batchChanges.rolloutWindows",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub batch_changes_rollout_windows: Vec<crate::rollout::RolloutWindowConfig>,

    /// Hosts and networks outbound webhooks may be sent to although they resolve to internal
    /// addresses. Without an entry here, deliveries to loopback, private, link-local,
    /// shared (CGNAT), cloud metadata and other non-public addresses are refused, after DNS
    /// resolution and on every redirect. Entries are host names (`hooks.internal`, or
    /// `*.corp.example.com` for its subdomains), IP addresses (`10.1.2.3`) or CIDR ranges
    /// (`10.0.0.0/8`, `fd00::/8`).
    #[serde(
        rename = "webhooks.outbound.allowedHosts",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub webhooks_outbound_allowed_hosts: Vec<String>,

    /// How long outbound webhook delivery logs, and the events they were for, are kept.
    #[serde(rename = "webhooks.outbound.logRetention")]
    pub webhooks_outbound_log_retention: ConfigDuration,

    /// Whether users other than site admins may make a watch deliver unfiltered results:
    /// every email recipient then gets every result the watch finds with its owner's access,
    /// including results in repositories the recipient cannot read. Site admins always may.
    /// Off (the default): each recipient gets only the results they can read.
    #[serde(rename = "watches.allowUnfilteredResults")]
    pub watches_allow_unfiltered_results: bool,

    /// How long search jobs are kept after they were created, e.g. `"720h"`. A worker
    /// singleton deletes finished jobs older than this with their results. Unset (the
    /// default): kept until their creator deletes them.
    #[serde(
        rename = "searchJobs.retention",
        skip_serializing_if = "Option::is_none"
    )]
    pub search_jobs_retention: Option<ConfigDuration>,

    /// Trends backfill: how many repository histories per second the
    /// backfiller and recorder count at most, per worker.
    #[serde(rename = "insights.query.worker.rateLimit")]
    #[schemars(range(min = 0.01))]
    pub insights_query_worker_rate_limit: f64,
    /// How many repository histories each worker counts at once for trend backfills.
    /// Read when the worker starts.
    #[serde(rename = "insights.query.worker.concurrency")]
    #[schemars(range(min = 1))]
    pub insights_query_worker_concurrency: u32,
    /// The most points a trend series keeps; older points are pruned. A new
    /// series is backfilled with this many points.
    #[serde(rename = "insights.maximumSampleSize")]
    #[schemars(range(min = 1))]
    pub insights_maximum_sample_size: u32,

    /// Sync teams from GitLab groups: every group a GitLab connection's token can see
    /// becomes a read-only team (nested groups as child teams) whose members are the group's
    /// direct members with a linked GitLab account. Runs hourly in the worker.
    #[serde(rename = "own.syncTeamsFromGitLabGroups")]
    pub own_sync_teams_from_gitlab_groups: bool,

    /// The deprecated `experimentalFeatures` object, read only for its
    /// `gitServerPinnedRepos` entry (see [`SiteConfig::pinned_repos`]). Not part of the
    /// schema; validation reports it with a deprecation warning.
    #[serde(
        rename = "experimentalFeatures",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(skip)]
    pub legacy_experimental_features: Option<Value>,
}

/// The deprecated site config key holding [`LEGACY_PINNED_REPOS_KEY`].
pub const LEGACY_EXPERIMENTAL_FEATURES_KEY: &str = "experimentalFeatures";
/// The deprecated `experimentalFeatures` entry that `repoNodes.pinnedRepos` replaces.
pub const LEGACY_PINNED_REPOS_KEY: &str = "gitServerPinnedRepos";
/// The site config key pinning repos to repo nodes.
pub const PINNED_REPOS_KEY: &str = "repoNodes.pinnedRepos";

/// The site config key of [`SiteConfig::search_index_always_index`].
pub const ALWAYS_INDEX_KEY: &str = "search.index.alwaysIndex";

/// The [`ALWAYS_INDEX_KEY`] entry that applies to every repository.
pub const ALL_REPOS: &str = "*";

impl SiteConfig {
    /// Repo name → repo node address: `repoNodes.pinnedRepos`, or, when that is empty, the
    /// deprecated `experimentalFeatures.gitServerPinnedRepos`, whose entries with a value that
    /// is not a string are skipped (validation warns about them). The rebalancer moves a
    /// pinned repo to its node and, once unpinned, back to its hash owner.
    pub fn always_index(&self, repo: &str) -> AlwaysIndex {
        let rules = &self.search_index_always_index;
        let patterns = [ALL_REPOS, repo]
            .into_iter()
            .filter_map(|k| rules.get(k))
            .flatten();
        // Validation rejects patterns that do not compile.
        AlwaysIndex::new(patterns).unwrap_or_default()
    }

    pub fn pinned_repos(&self) -> BTreeMap<String, String> {
        if !self.repo_nodes_pinned_repos.is_empty() {
            return self.repo_nodes_pinned_repos.clone();
        }
        let Some(Value::Object(map)) = self
            .legacy_experimental_features
            .as_ref()
            .and_then(|f| f.get(LEGACY_PINNED_REPOS_KEY))
        else {
            return BTreeMap::new();
        };
        map.iter()
            .filter_map(|(name, node)| Some((name.clone(), node.as_str()?.to_owned())))
            .collect()
    }
}

impl Default for SiteConfig {
    fn default() -> Self {
        Self {
            external_url: None,
            auth_providers: vec![AuthProvider::Builtin(BuiltinAuthProvider::default())],
            auth_session_expiry: ConfigDuration::from_secs(30 * 86_400),
            auth_password_reset_link_expiry: 4 * 3600,
            auth_allowed_ip_address: AllowedIpAddress::default(),
            email_smtp: None,
            email_address: None,
            email_sender_name: None,
            auth_password_policy: PasswordPolicy::default(),
            auth_lockout: Lockout::default(),
            auth_rate_limits: AuthRateLimits::default(),
            auth_access_request: AccessRequest::default(),
            auth_access_tokens: AccessTokens::default(),
            auth_public: false,
            auth_enable_username_changes: false,
            mcp_oauth_enabled: true,
            mcp_oauth_dynamic_client_registration: true,
            mcp_oauth_access_token_ttl: ConfigDuration::from_secs(3600),
            mcp_oauth_refresh_token_ttl: ConfigDuration::from_secs(30 * 86_400),
            mcp_oauth_allowed_redirect_hosts: Vec::new(),
            permissions_user_mapping: PermissionsUserMapping::default(),
            authz_enforce_for_site_admins: false,
            permissions_sync_schedule_interval: 15,
            permissions_sync_oldest_users: 10,
            permissions_sync_oldest_repos: 10,
            permissions_sync_users_backoff_seconds: 60,
            permissions_sync_repos_backoff_seconds: 60,
            permissions_sync_jobs_history_size: 5,
            log: LogConfig::default(),
            event_logs_retention: ConfigDuration::from_secs(93 * 86_400),
            event_logs_rollup_retention: ConfigDuration::from_secs(730 * 86_400),
            observability_log_slow_graphql_requests: None,
            observability_log_slow_searches: None,
            outbound_request_log_limit: 0,
            search_limits: SearchLimits::default(),
            search_index_branches: BTreeMap::new(),
            search_index_always_index: BTreeMap::new(),
            repo_list_update_interval: 1,
            disable_auto_git_updates: false,
            git_update_interval: Vec::new(),
            git_max_concurrent_clones: 5,
            repo_nodes_pinned_repos: BTreeMap::new(),
            executors_access_token: None,
            scim_auth_token: None,
            code_intel_gitlab_job_token_uploads: false,
            code_intel_max_upload_bytes: 2 << 30,
            codeintel_ranking_enabled: true,
            code_intel_auto_indexing_enabled: false,
            code_intel_auto_indexing_allow_global_policies: false,
            code_intel_auto_indexing_policy_repository_match_limit: -1,
            code_intel_auto_indexing_indexer_map: BTreeMap::new(),
            code_intel_auto_indexing_repository_commits_per_hour: 20,
            batch_changes_enforce_forks: false,
            batch_changes_auto_delete_branch: false,
            batch_changes_reconciler_concurrency: 5,
            batch_changes_enabled: true,
            batch_changes_restrict_to_admins: false,
            batch_changes_commit_via_api: false,
            batch_changes_reject_unverified_commit: false,
            batch_changes_commit_via_api_max_bytes: 10 * 1024 * 1024,
            batch_changes_commit_via_api_max_files: 1000,
            batch_changes_changesets_retention: None,
            batch_changes_rollout_windows: Vec::new(),
            webhooks_outbound_allowed_hosts: Vec::new(),
            webhooks_outbound_log_retention: ConfigDuration::from_secs(7 * 86_400),
            watches_allow_unfiltered_results: false,
            search_jobs_retention: None,
            insights_query_worker_rate_limit: 20.0,
            insights_query_worker_concurrency: 1,
            insights_maximum_sample_size: 30,
            own_sync_teams_from_gitlab_groups: false,
            legacy_experimental_features: None,
        }
    }
}

/// Client IP allowlist with explicit trusted proxy networks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, rename_all = "camelCase")]
pub struct AllowedIpAddress {
    pub enabled: bool,
    pub user_ip_address: Vec<String>,
    pub trusted_client_ip_address: Vec<String>,
    #[serde(default = "default_client_ip_header")]
    pub client_ip_request_header: String,
}

impl Default for AllowedIpAddress {
    fn default() -> Self {
        Self {
            enabled: false,
            user_ip_address: Vec::new(),
            trusted_client_ip_address: Vec::new(),
            client_ip_request_header: default_client_ip_header(),
        }
    }
}

fn default_client_ip_header() -> String {
    "X-Forwarded-For".into()
}

/// SMTP server configuration. `noVerifyTLS` permits self-signed server certificates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SmtpConfig {
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
    pub password: Option<String>,
    #[serde(default)]
    pub authentication: SmtpAuthentication,
    #[serde(rename = "noVerifyTLS", default)]
    pub no_verify_tls: bool,
    #[serde(default)]
    pub encryption: SmtpEncryption,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
pub enum SmtpAuthentication {
    #[default]
    #[serde(rename = "none")]
    None,
    #[serde(rename = "PLAIN")]
    Plain,
    #[serde(rename = "LOGIN")]
    Login,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum SmtpEncryption {
    #[default]
    Starttls,
    Tls,
    None,
}

/// One `gitUpdateInterval` rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GitUpdateIntervalRule {
    /// Regular expression matched against the repo name, e.g. `^gitlab\.com/big-group/`.
    pub pattern: String,
    /// Minutes between fetches of matching repos.
    #[schemars(range(min = 1))]
    pub interval: u32,
}

/// An authentication provider, tagged by its `"type"` field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type")]
pub enum AuthProvider {
    /// Built-in username/password accounts.
    #[serde(rename = "builtin")]
    Builtin(BuiltinAuthProvider),
    /// GitLab OAuth (gitlab.com or self-managed).
    #[serde(rename = "gitlab")]
    Gitlab(GitlabAuthProvider),
    /// GitHub OAuth (github.com or GitHub Enterprise Server).
    #[serde(rename = "github")]
    Github(GithubAuthProvider),
    /// Gitea OAuth.
    #[serde(rename = "gitea")]
    Gitea(GiteaAuthProvider),
    /// OpenID Connect.
    #[serde(rename = "openidconnect")]
    OpenIdConnect(OpenIdConnectAuthProvider),
    /// Identity taken from HTTP headers set by a trusted authenticating proxy.
    #[serde(rename = "http-header")]
    HttpHeader(HttpHeaderAuthProvider),
}

/// Built-in username/password provider.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BuiltinAuthProvider {
    /// Allow anyone to create an account. When false, only admins can create users.
    #[serde(default)]
    pub allow_signup: bool,
}

/// GitLab OAuth provider. Create an OAuth application in GitLab with the redirect URI
/// `<externalURL>/.auth/gitlab/callback`, "Confidential" checked and the scopes `read_user`
/// and `read_api`, then copy its Application ID and Secret here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GitlabAuthProvider {
    /// GitLab instance URL (default `https://gitlab.com/`).
    #[serde(default = "default_gitlab_url")]
    pub url: String,
    /// The GitLab OAuth application's "Application ID". The application needs the redirect
    /// URI `<externalURL>/.auth/gitlab/callback` and the scopes `read_user` and `read_api`.
    #[serde(rename = "clientID")]
    pub client_id: String,
    /// The GitLab OAuth application's "Secret".
    pub client_secret: String,
    /// Create an account on first sign-in.
    #[serde(default)]
    pub allow_signup: bool,
    /// Link a first sign-in to the existing user with the same verified email (unset: `true`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link_by_email: Option<bool>,
    /// If non-empty, only members of these groups (full paths) may sign in.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow_groups: Vec<String>,
    /// Label on the sign-in button.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
}

fn default_gitlab_url() -> String {
    "https://gitlab.com/".to_string()
}

/// GitHub OAuth provider. Create an OAuth app in GitHub (Settings → Developer settings → OAuth
/// Apps, for a user or an organization) with the callback URL
/// `<externalURL>/.auth/github/callback`, then copy its Client ID and a client secret here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GithubAuthProvider {
    /// GitHub URL: `https://github.com/` (the default) or a GitHub Enterprise Server URL.
    #[serde(default = "default_github_url")]
    pub url: String,
    /// The GitHub OAuth app's "Client ID". The app needs the callback URL
    /// `<externalURL>/.auth/github/callback`.
    #[serde(rename = "clientID")]
    pub client_id: String,
    /// One of the GitHub OAuth app's client secrets.
    pub client_secret: String,
    /// Create an account on first sign-in.
    #[serde(default)]
    pub allow_signup: bool,
    /// Link a first sign-in to the existing user with the same verified email (unset: `true`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link_by_email: Option<bool>,
    /// If non-empty, only members of these organizations (logins) may sign in.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow_orgs: Vec<String>,
    /// Label on the sign-in button.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
}

fn default_github_url() -> String {
    "https://github.com/".to_string()
}

/// Gitea OAuth provider. Create an OAuth2 application in Gitea (Settings → Applications, or
/// Site Administration → Applications for the whole instance) with the redirect URI
/// `<externalURL>/.auth/gitea/callback` and "Confidential client" checked, then copy its
/// Client ID and Client Secret here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GiteaAuthProvider {
    /// Gitea instance URL, e.g. `https://gitea.example.com/`.
    pub url: String,
    /// The Gitea OAuth2 application's "Client ID". The application needs the redirect URI
    /// `<externalURL>/.auth/gitea/callback`.
    #[serde(rename = "clientID")]
    pub client_id: String,
    /// The Gitea OAuth2 application's "Client Secret".
    pub client_secret: String,
    /// Create an account on first sign-in.
    #[serde(default)]
    pub allow_signup: bool,
    /// Link a first sign-in to the existing user with the same verified email (unset: `true`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link_by_email: Option<bool>,
    /// If non-empty, only members of these organizations may sign in.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow_orgs: Vec<String>,
    /// Label on the sign-in button.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
}

/// OpenID Connect provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct OpenIdConnectAuthProvider {
    /// Stable identifier used by the sign-in URL (defaults to the client ID).
    #[serde(rename = "configID", default, skip_serializing_if = "Option::is_none")]
    pub config_id: Option<String>,
    /// Issuer URL; discovery is done at `<issuer>/.well-known/openid-configuration`.
    pub issuer: String,
    /// Client ID.
    #[serde(rename = "clientID")]
    pub client_id: String,
    /// Client secret.
    pub client_secret: String,
    /// Only allow users whose verified email is in this domain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub require_email_domain: Option<String>,
    /// Create an account on first sign-in.
    #[serde(default)]
    pub allow_signup: bool,
    /// Link a first sign-in to the existing user with the same verified email (unset: `true`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link_by_email: Option<bool>,
    /// If non-empty, only members of these groups may sign in.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow_groups: Vec<String>,
    /// Claim containing a list of group names.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub groups_claim: Option<String>,
    /// Label on the sign-in button.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
}

/// HTTP header provider for use behind an authenticating proxy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HttpHeaderAuthProvider {
    /// Header carrying the username.
    pub username_header: String,
    /// Header carrying the email address.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email_header: Option<String>,
    /// Prefix removed from the username header before normalization.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strip_username_header_prefix: Option<String>,
    /// CIDRs of authenticating proxies. An empty list trusts no peer (fail closed).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub trusted_proxies: Vec<String>,
}

/// Access request settings. An omitted `enabled` follows the built-in sign-up policy.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, rename_all = "camelCase")]
pub struct AccessRequest {
    /// Override whether anonymous visitors can submit requests.
    pub enabled: Option<bool>,
}

/// Password rules for built-in accounts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, rename_all = "camelCase")]
pub struct PasswordPolicy {
    /// Enforce the character-class rules below. The minimum length always applies.
    pub enabled: bool,
    /// Minimum password length.
    #[schemars(range(min = 8))]
    pub minimum_length: u32,
    /// Required number of special characters.
    pub number_of_special_characters: u32,
    /// Require at least one digit.
    pub require_at_least_one_number: bool,
    /// Require both upper- and lower-case letters.
    pub require_upperand_lower_case: bool,
}

impl Default for PasswordPolicy {
    fn default() -> Self {
        Self {
            enabled: false,
            minimum_length: 12,
            number_of_special_characters: 0,
            require_at_least_one_number: false,
            require_upperand_lower_case: false,
        }
    }
}

/// Account lockout policy. Periods are in seconds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, rename_all = "camelCase")]
pub struct Lockout {
    /// Window (seconds) from the first failed attempt of a run in which failures
    /// are counted together. A failure after the window starts a new count at 1.
    pub consecutive_period: u64,
    /// Failed attempts within the window that lock the account.
    #[schemars(range(min = 1))]
    pub failed_attempt_threshold: u32,
    /// How long (seconds) the account stays locked.
    pub lockout_period: u64,
}

impl Default for Lockout {
    fn default() -> Self {
        Self {
            consecutive_period: 3600,
            failed_attempt_threshold: 5,
            lockout_period: 1800,
        }
    }
}

/// Request limits on the endpoints anyone can call. Over a limit the endpoint answers 429
/// with `Retry-After`. Counters are shared by every process.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, rename_all = "camelCase")]
pub struct AuthRateLimits {
    /// Apply the limits below.
    pub enabled: bool,
    /// Password sign-in attempts per client address.
    pub sign_in_per_address: RequestLimit,
    /// Failed password sign-ins per account (username or email, from any address).
    pub failed_sign_ins_per_account: RequestLimit,
    /// Password reset requests per client address.
    pub password_reset_per_address: RequestLimit,
    /// Password reset requests per email address.
    pub password_reset_per_email: RequestLimit,
    /// Access requests per client address.
    pub access_request_per_address: RequestLimit,
    /// Access requests per email address.
    pub access_request_per_email: RequestLimit,
    /// OAuth authorization requests (`/.oauth/authorize`) per client address.
    pub oauth_authorize_per_address: RequestLimit,
    /// Rejected incoming code host webhook deliveries per client address.
    pub failed_webhooks_per_address: RequestLimit,
}

impl Default for AuthRateLimits {
    fn default() -> Self {
        Self {
            enabled: true,
            sign_in_per_address: RequestLimit::new(50, 300),
            failed_sign_ins_per_account: RequestLimit::new(100, 3600),
            password_reset_per_address: RequestLimit::new(10, 3600),
            password_reset_per_email: RequestLimit::new(3, 3600),
            access_request_per_address: RequestLimit::new(10, 3600),
            access_request_per_email: RequestLimit::new(3, 3600),
            oauth_authorize_per_address: RequestLimit::new(300, 300),
            failed_webhooks_per_address: RequestLimit::new(100, 300),
        }
    }
}

/// `requests` per `period` seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RequestLimit {
    /// Requests allowed per period.
    pub requests: u32,
    /// The period in seconds.
    #[schemars(range(min = 1))]
    pub period: u64,
}

impl RequestLimit {
    /// `requests` per `period` seconds.
    pub const fn new(requests: u32, period: u64) -> Self {
        Self { requests, period }
    }
}

/// Access token policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, rename_all = "camelCase")]
pub struct AccessTokens {
    /// Who may create access tokens.
    pub allow: AccessTokenAllow,
    /// Maximum lifetime of a new token, in days.
    #[schemars(range(min = 1))]
    pub max_token_expires_in_days: u32,
}

impl Default for AccessTokens {
    fn default() -> Self {
        Self {
            allow: AccessTokenAllow::All,
            max_token_expires_in_days: 365,
        }
    }
}

/// Who may create access tokens.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum AccessTokenAllow {
    /// Any user may create tokens for themselves.
    #[default]
    All,
    /// Only site admins may create tokens (for any user).
    SiteAdminCreate,
    /// Access tokens are disabled.
    None,
}

/// `permissions.userMapping`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, rename_all = "camelCase")]
pub struct PermissionsUserMapping {
    /// Turn on explicit permissions.
    pub enabled: bool,
    /// How the API names users: by verified email address or by username.
    #[serde(rename = "bindID")]
    pub bind_id: BindId,
}

/// `permissions.userMapping.bindID`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum BindId {
    /// A verified email address.
    #[default]
    Email,
    /// A username.
    Username,
}

/// Logging settings.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, rename_all = "camelCase")]
pub struct LogConfig {
    /// Where security events (sign-ins, token use, admin actions) are recorded.
    pub security_event_log: SecurityEventLog,
}

/// Security event log settings.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, rename_all = "camelCase")]
pub struct SecurityEventLog {
    /// Destination of security events.
    pub location: SecurityEventLogLocation,
}

/// Destination of security events.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum SecurityEventLogLocation {
    /// The `security_event_logs` table.
    #[default]
    Database,
    /// Structured application logs.
    Logs,
    /// Both.
    All,
}

/// Search limits and timeouts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, rename_all = "camelCase")]
pub struct SearchLimits {
    /// Repositories one part of a search spans at most; the rest are skipped with a
    /// warning. Unset means no limit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_repos: Option<u32>,
    /// Default result limit for streaming search (`count:` overrides it).
    pub streaming_max_results: u32,
    /// Result limit of a search job.
    pub exhaustive_max_results: u32,
    /// Default search timeout in seconds.
    #[schemars(range(min = 1))]
    pub default_timeout_seconds: u32,
    /// Maximum value of `timeout:` in seconds; longer values are clamped to it.
    #[schemars(range(min = 1))]
    pub max_timeout_seconds: u32,
    /// How long a search job may search one repository revision, in seconds.
    #[schemars(range(min = 1))]
    pub exhaustive_timeout_seconds: u32,
    /// Maximum repos for `type:commit`/`type:diff` searches without a time filter.
    pub commit_diff_max_repos: u32,
    /// Maximum repos for `type:commit`/`type:diff` searches with `before:`/`after:`.
    pub commit_diff_with_time_filter_max_repos: u32,
}

impl Default for SearchLimits {
    fn default() -> Self {
        Self {
            max_repos: None,
            streaming_max_results: 500,
            exhaustive_max_results: 1_000_000,
            default_timeout_seconds: 20,
            max_timeout_seconds: 60,
            exhaustive_timeout_seconds: 3600,
            commit_diff_max_repos: 50,
            commit_diff_with_time_filter_max_repos: 10_000,
        }
    }
}
