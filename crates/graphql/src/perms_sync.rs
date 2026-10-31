//! The code host permissions sync:
//! `scheduleUserPermissionsSync`, `scheduleRepositoryPermissionsSync`,
//! `cancelPermissionsSyncJob`, `permissionsSyncJobs`, and `User.permissionsInfo`. Site admins
//! only.

use async_graphql::{Context, Enum, ID, Object, Result, SimpleObject, Union};
use chrono::{DateTime, Utc};
use serde_json::json;
use sourcegull_db::{ListPermsSyncJobs, PermsSyncJobRecord, PermsSyncReason, PermsSyncSubject};
use sourcegull_services::relay::{marshal_id, unmarshal_int_id};
use sourcegull_types::{RepoAccess, UserId};

use crate::EmptyResponse;
use crate::audit::{self, events};
use crate::context::{codes, db_error, error, page_size, require_site_admin, services, to_int};
use crate::pagination::{PageInfo, decode_cursor};
use crate::repos::{RepositoryNode, parse_repository_id};
use crate::users::{UserNode, parse_user_id};

const JOB_KIND: &str = "PermissionsSyncJob";

/// The state of a permissions sync job.
#[derive(Enum, Copy, Clone, Debug, PartialEq, Eq)]
pub enum PermissionsSyncJobState {
    Queued,
    Processing,
    Errored,
    Failed,
    Completed,
    Canceled,
}

impl PermissionsSyncJobState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Processing => "processing",
            Self::Errored => "errored",
            Self::Failed => "failed",
            Self::Completed => "completed",
            Self::Canceled => "canceled",
        }
    }
}

/// How urgent a job is.
#[derive(Enum, Copy, Clone, Debug, PartialEq, Eq)]
pub enum PermissionsSyncJobPriority {
    Low,
    Medium,
    High,
}

/// The outcome of a job on one code host connection.
#[derive(SimpleObject)]
pub struct CodeHostState {
    /// The code host connection's ID.
    #[graphql(name = "providerID")]
    pub provider_id: ID,
    /// The code host type: `gitlab`, `github` or `gitea`.
    pub provider_type: String,
    /// `SUCCESS` or `ERROR`.
    pub status: String,
    pub message: String,
}

/// Whose permissions a job synced.
// Values are short-lived (one per resolved field), so the size difference does not matter.
#[allow(clippy::large_enum_variant)]
#[derive(Union)]
pub enum PermissionsSyncJobSubject {
    Repository(RepositoryNode),
    User(UserNode),
}

/// One permissions sync run.
pub struct PermissionsSyncJobNode(pub PermsSyncJobRecord);

#[Object(name = "PermissionsSyncJob")]
impl PermissionsSyncJobNode {
    /// The opaque node ID.
    async fn id(&self) -> ID {
        ID(marshal_id(JOB_KIND, self.0.id))
    }

    async fn state(&self) -> PermissionsSyncJobState {
        match self.0.state.as_str() {
            "queued" => PermissionsSyncJobState::Queued,
            "processing" => PermissionsSyncJobState::Processing,
            "errored" => PermissionsSyncJobState::Errored,
            "failed" => PermissionsSyncJobState::Failed,
            "canceled" => PermissionsSyncJobState::Canceled,
            _ => PermissionsSyncJobState::Completed,
        }
    }

    /// Why the job was queued (`user_signed_in`, `repo_created`, `manual`, `webhook`,
    /// `scheduled`).
    async fn reason(&self) -> &str {
        &self.0.reason
    }

    async fn priority(&self) -> PermissionsSyncJobPriority {
        match self.0.priority {
            p if p >= PermsSyncReason::RepoCreated.priority() => PermissionsSyncJobPriority::High,
            p if p >= PermsSyncReason::Manual.priority() => PermissionsSyncJobPriority::Medium,
            _ => PermissionsSyncJobPriority::Low,
        }
    }

    async fn failure_message(&self) -> Option<&str> {
        self.0.failure_message.as_deref()
    }

    async fn queued_at(&self) -> DateTime<Utc> {
        self.0.queued_at
    }

    async fn started_at(&self) -> Option<DateTime<Utc>> {
        self.0.started_at
    }

    async fn finished_at(&self) -> Option<DateTime<Utc>> {
        self.0.finished_at
    }

    /// Milliseconds the run took.
    async fn ran_for_ms(&self) -> Option<i32> {
        let (start, end) = (self.0.started_at?, self.0.finished_at?);
        i32::try_from((end - start).num_milliseconds()).ok()
    }

    /// Whether cancellation was requested.
    async fn cancel(&self) -> bool {
        self.0.cancel
    }

    /// Grants added.
    async fn permissions_added(&self) -> i32 {
        self.0.added
    }

    /// Grants removed.
    async fn permissions_removed(&self) -> i32 {
        self.0.removed
    }

    /// Repositories (for a user) or users (for a repository) the code hosts returned.
    async fn permissions_found(&self) -> i32 {
        self.0.found
    }

    /// Some code hosts failed while others succeeded.
    async fn partial_success(&self) -> bool {
        let states = self.code_host_states_vec();
        states.iter().any(|s| s.status == "ERROR") && states.iter().any(|s| s.status == "SUCCESS")
    }

    /// The outcome per code host connection.
    async fn code_host_states(&self) -> Vec<CodeHostState> {
        self.code_host_states_vec()
    }

    /// The admin who asked for the job, if one did.
    async fn triggered_by_user(&self, ctx: &Context<'_>) -> Result<Option<UserNode>> {
        let Some(id) = self.0.triggered_by_user_id else {
            return Ok(None);
        };
        let user = services(ctx)?
            .db
            .users()
            .get_by_id(id)
            .await
            .map_err(db_error)?;
        Ok(user.map(UserNode))
    }

    /// The user or repository whose permissions were synced (null once deleted).
    async fn subject(&self, ctx: &Context<'_>) -> Result<Option<PermissionsSyncJobSubject>> {
        let db = &services(ctx)?.db;
        if let Some(u) = self.0.user_id {
            let user = db.users().get_by_id(u).await.map_err(db_error)?;
            return Ok(user.map(|u| PermissionsSyncJobSubject::User(UserNode(u))));
        }
        if let Some(r) = self.0.repository_id {
            let repo = db
                .repos()
                .get_authorized(r, &RepoAccess::All)
                .await
                .map_err(db_error)?;
            return Ok(repo.map(|repo| {
                PermissionsSyncJobSubject::Repository(RepositoryNode {
                    repo,
                    gitserver: None,
                })
            }));
        }
        Ok(None)
    }
}

impl PermissionsSyncJobNode {
    fn code_host_states_vec(&self) -> Vec<CodeHostState> {
        self.0
            .code_host_states
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|s| CodeHostState {
                        provider_id: ID(marshal_id(
                            sourcegull_services::relay::EXTERNAL_SERVICE_KIND,
                            s["externalServiceID"].as_i64().unwrap_or_default(),
                        )),
                        // Jobs recorded before states named their code host were all GitLab.
                        provider_type: s["serviceType"].as_str().unwrap_or("gitlab").to_owned(),
                        status: if s["status"] == "ok" {
                            "SUCCESS"
                        } else {
                            "ERROR"
                        }
                        .to_owned(),
                        message: s["message"].as_str().unwrap_or_default().to_owned(),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// A page of permissions sync jobs.
#[derive(SimpleObject)]
pub struct PermissionsSyncJobsConnection {
    pub nodes: Vec<PermissionsSyncJobNode>,
    pub total_count: i32,
    pub page_info: PageInfo,
}

/// A user's code host permissions state.
#[derive(SimpleObject)]
pub struct UserPermissionsInfo {
    /// When the user's code host permissions were last synced.
    pub synced_at: Option<DateTime<Utc>>,
}

/// A user's account on a code host.
#[derive(SimpleObject)]
#[graphql(name = "ExternalAccount")]
pub struct ExternalAccountNode {
    pub id: ID,
    /// `gitlab`, `github` or `gitea`.
    pub service_type: String,
    /// The code host instance URL.
    #[graphql(name = "serviceID")]
    pub service_id: String,
    /// The OAuth application of the stored token ('' without one).
    #[graphql(name = "clientID")]
    pub client_id: String,
    /// The account id on the code host.
    #[graphql(name = "accountID")]
    pub account_id: String,
    /// The username on the code host.
    pub username: Option<String>,
    /// Whether an OAuth token is stored and has not stopped working.
    pub has_valid_token: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// A user's linked accounts.
#[derive(SimpleObject)]
pub struct ExternalAccountConnection {
    pub nodes: Vec<ExternalAccountNode>,
    pub total_count: i32,
}

/// `User.externalAccounts`; the caller checks access.
pub(crate) async fn user_external_accounts(
    ctx: &Context<'_>,
    user: UserId,
) -> Result<ExternalAccountConnection> {
    let rows = services(ctx)?
        .db
        .external_accounts(None)
        .list_summaries(user)
        .await
        .map_err(db_error)?;
    let nodes: Vec<ExternalAccountNode> = rows
        .into_iter()
        .map(|a| ExternalAccountNode {
            id: ID(marshal_id("ExternalAccount", a.id.get())),
            service_type: a.service_type,
            service_id: a.service_id,
            client_id: a.client_id,
            account_id: a.account_id,
            username: a.account_data["username"].as_str().map(str::to_owned),
            has_valid_token: a.has_token && a.expired_at.is_none(),
            created_at: a.created_at,
            updated_at: a.updated_at,
        })
        .collect();
    Ok(ExternalAccountConnection {
        total_count: to_int(i64::try_from(nodes.len()).unwrap_or(i64::MAX)),
        nodes,
    })
}

/// Why a user has to sign in with a code host again.
#[derive(Enum, Copy, Clone, Debug, PartialEq, Eq)]
pub enum CodeHostSignInReason {
    /// The code host rejected the stored token (revoked, or its refresh failed).
    Expired,
    /// The token lacks the scope the permissions sync needs to list private repositories.
    MissingScope,
}

/// A code host the user has to sign in with again before the permissions sync can see their
/// private repositories there.
#[derive(SimpleObject, Debug, PartialEq)]
pub struct CodeHostSignIn {
    /// `gitlab`, `github` or `gitea`.
    pub service_type: String,
    /// The code host instance URL.
    #[graphql(name = "serviceID")]
    pub service_id: String,
    /// The name to show: the sign-in provider's display name, or the code host's (`GitHub`).
    pub display_name: String,
    pub reason: CodeHostSignInReason,
    /// Starts sign-in with the matching `auth.providers` entry (append `&returnTo=`); null
    /// when none is configured.
    #[graphql(name = "authenticationURL")]
    pub authentication_url: Option<String>,
}

/// `User.codeHostSignInsNeeded`: the user's accounts the code host rejected, and the code
/// hosts whose latest user permissions sync found the token without a needed scope (unless the
/// user signed in again since). The caller checks access.
pub(crate) async fn user_code_host_sign_ins(
    ctx: &Context<'_>,
    user: UserId,
) -> Result<Vec<CodeHostSignIn>> {
    let svc = services(ctx)?;
    let accounts = svc
        .db
        .external_accounts(None)
        .list_summaries(user)
        .await
        .map_err(db_error)?;
    let mut needed: Vec<(String, String, CodeHostSignInReason)> = accounts
        .iter()
        .filter(|a| a.expired_at.is_some())
        .map(|a| {
            (
                a.service_type.clone(),
                a.service_id.clone(),
                CodeHostSignInReason::Expired,
            )
        })
        .collect();
    let jobs = svc
        .db
        .perms_sync_jobs()
        .list(&ListPermsSyncJobs {
            subject: Some(PermsSyncSubject::User(user)),
            limit: 10,
            ..Default::default()
        })
        .await
        .map_err(db_error)?;
    if let Some(job) = jobs.iter().find(|j| j.finished_at.is_some()) {
        let finished = job.finished_at.unwrap_or_default();
        for s in job.code_host_states.as_array().into_iter().flatten() {
            if s["signIn"] != "missingScope" {
                continue;
            }
            let (Some(ty), Some(id)) = (s["serviceType"].as_str(), s["serviceID"].as_str()) else {
                continue;
            };
            let signed_in_since = accounts
                .iter()
                .any(|a| a.service_type == ty && a.service_id == id && a.updated_at > finished);
            if !signed_in_since && !needed.iter().any(|(t, i, _)| t == ty && i == id) {
                needed.push((
                    ty.to_owned(),
                    id.to_owned(),
                    CodeHostSignInReason::MissingScope,
                ));
            }
        }
    }
    let config = svc.config.load();
    Ok(needed
        .into_iter()
        .map(|(service_type, service_id, reason)| {
            let provider =
                sign_in_provider(&config.value.auth_providers, &service_type, &service_id);
            let host = service_type
                .parse::<sourcegull_types::ExternalServiceKind>()
                .map_or_else(|_| service_type.clone(), |k| k.name().to_owned());
            CodeHostSignIn {
                display_name: provider
                    .and_then(|(_, name)| name.map(str::to_owned))
                    .unwrap_or(host),
                authentication_url: provider.map(|(client_id, _)| {
                    format!(
                        "/.auth/{service_type}/login?pc={}",
                        url::form_urlencoded::byte_serialize(client_id.as_bytes())
                            .collect::<String>()
                    )
                }),
                service_type,
                service_id,
                reason,
            }
        })
        .collect())
}

/// The client id and display name of the `auth.providers` entry of type `service_type` for
/// the instance at `service_id`.
fn sign_in_provider<'a>(
    providers: &'a [sourcegull_config::AuthProvider],
    service_type: &str,
    service_id: &str,
) -> Option<(&'a str, Option<&'a str>)> {
    use sourcegull_config::AuthProvider;
    let same = |url: &str| {
        url.trim()
            .trim_end_matches('/')
            .eq_ignore_ascii_case(service_id.trim_end_matches('/'))
    };
    providers.iter().find_map(|p| {
        let (ty, url, client_id, name) = match p {
            AuthProvider::Gitlab(g) => ("gitlab", &g.url, &g.client_id, &g.display_name),
            AuthProvider::Github(g) => ("github", &g.url, &g.client_id, &g.display_name),
            AuthProvider::Gitea(g) => ("gitea", &g.url, &g.client_id, &g.display_name),
            _ => return None,
        };
        (ty == service_type && same(url)).then_some((client_id.as_str(), name.as_deref()))
    })
}

/// `User.permissionsSyncInProgress`; false for site admins who read every repository anyway.
/// The caller checks access.
pub(crate) async fn user_sync_in_progress(
    ctx: &Context<'_>,
    user: &sourcegull_types::User,
) -> Result<bool> {
    let svc = services(ctx)?;
    if user.site_admin && !svc.config.load().value.authz_enforce_for_site_admins {
        return Ok(false);
    }
    svc.db
        .perms_sync_jobs()
        .user_sync_in_progress(user.id)
        .await
        .map_err(db_error)
}

/// `User.permissionsInfo` (site admins only).
pub(crate) async fn user_permissions_info(
    ctx: &Context<'_>,
    user: UserId,
) -> Result<UserPermissionsInfo> {
    require_site_admin(ctx)?;
    let synced_at = services(ctx)?
        .db
        .perms_sync_jobs()
        .user_synced_at(user)
        .await
        .map_err(db_error)?;
    Ok(UserPermissionsInfo { synced_at })
}

/// Permissions sync queries.
#[derive(Default)]
pub struct PermsSyncQuery;

#[Object]
impl PermsSyncQuery {
    /// Permissions sync jobs, newest first, optionally of one user or repository and in one
    /// state (site admins only).
    async fn permissions_sync_jobs(
        &self,
        ctx: &Context<'_>,
        first: Option<i32>,
        after: Option<String>,
        #[graphql(name = "userID")] user_id: Option<ID>,
        #[graphql(name = "repoID")] repo_id: Option<ID>,
        state: Option<PermissionsSyncJobState>,
    ) -> Result<PermissionsSyncJobsConnection> {
        require_site_admin(ctx)?;
        let subject = match (user_id, repo_id) {
            (Some(u), None) => Some(PermsSyncSubject::User(parse_user_id(&u)?)),
            (None, Some(r)) => Some(PermsSyncSubject::Repo(parse_repository_id(&r)?)),
            (None, None) => None,
            _ => {
                return Err(error(
                    codes::INVALID_INPUT,
                    "Pass at most one of userID and repoID.",
                ));
            }
        };
        let limit = page_size(first, 50, 500);
        let offset = decode_cursor(after.as_deref())?;
        let mut filter = ListPermsSyncJobs {
            subject,
            state: state.map(|s| s.as_str().to_owned()),
            limit: limit + 1,
            offset,
        };
        let store = services(ctx)?.db.perms_sync_jobs();
        let rows = store.list(&filter).await.map_err(db_error)?;
        filter.limit = 0;
        let total = store.count(&filter).await.map_err(db_error)?;
        let page_info = PageInfo::new(offset, limit, rows.len());
        Ok(PermissionsSyncJobsConnection {
            nodes: rows
                .into_iter()
                .take(usize::try_from(limit).unwrap_or(0))
                .map(PermissionsSyncJobNode)
                .collect(),
            total_count: to_int(total),
            page_info,
        })
    }
}

/// Permissions sync mutations.
#[derive(Default)]
pub struct PermsSyncMutation;

#[Object]
impl PermsSyncMutation {
    /// Queues a sync of the repositories `user` can read on the code hosts (site admins only).
    async fn schedule_user_permissions_sync(
        &self,
        ctx: &Context<'_>,
        user: ID,
    ) -> Result<EmptyResponse> {
        let actor = require_site_admin(ctx)?;
        let svc = services(ctx)?;
        let id = parse_user_id(&user)?;
        if svc
            .db
            .users()
            .get_by_id(id)
            .await
            .map_err(db_error)?
            .is_none()
        {
            return Err(error(codes::NOT_FOUND, "User not found."));
        }
        schedule(svc, PermsSyncSubject::User(id), actor.user_id).await
    }

    /// Queues a sync of the users who can read `repository` on its code host (site admins
    /// only).
    async fn schedule_repository_permissions_sync(
        &self,
        ctx: &Context<'_>,
        repository: ID,
    ) -> Result<EmptyResponse> {
        let actor = require_site_admin(ctx)?;
        let svc = services(ctx)?;
        let id = parse_repository_id(&repository)?;
        if svc
            .db
            .repos()
            .get_authorized(id, &RepoAccess::All)
            .await
            .map_err(db_error)?
            .is_none()
        {
            return Err(error(codes::NOT_FOUND, "Repository not found."));
        }
        schedule(svc, PermsSyncSubject::Repo(id), actor.user_id).await
    }

    /// Cancels a queued or running job (site admins only).
    async fn cancel_permissions_sync_job(
        &self,
        ctx: &Context<'_>,
        job: ID,
    ) -> Result<EmptyResponse> {
        require_site_admin(ctx)?;
        let id = unmarshal_int_id(JOB_KIND, &job)
            .ok_or_else(|| error(codes::INVALID_INPUT, "Invalid permissions sync job ID."))?;
        let found = services(ctx)?
            .db
            .perms_sync_jobs()
            .cancel(id)
            .await
            .map_err(db_error)?;
        if !found {
            return Err(error(
                codes::NOT_FOUND,
                "No queued or running permissions sync job with this ID.",
            ));
        }
        Ok(EmptyResponse)
    }
}

async fn schedule(
    svc: &crate::Services,
    subject: PermsSyncSubject,
    actor: UserId,
) -> Result<EmptyResponse> {
    let job = svc
        .db
        .perms_sync_jobs()
        .enqueue(subject, PermsSyncReason::Manual, Some(actor))
        .await
        .map_err(db_error)?;
    if job.is_none() {
        return Err(error(
            codes::INVALID_INPUT,
            "No code host connection has an \"authorization\" block, so there is nothing to sync.",
        ));
    }
    let target = match subject {
        PermsSyncSubject::User(u) => json!({ "user": u }),
        PermsSyncSubject::Repo(r) => json!({ "repo": r }),
    };
    audit::record(svc, events::PERMISSIONS_SYNC_SCHEDULED, Some(actor), target).await;
    Ok(EmptyResponse)
}
