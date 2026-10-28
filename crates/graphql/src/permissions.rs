//! Explicit repository permissions:
//! `setRepositoryPermissionsForUsers`, `setRepositoryPermissionsUnrestricted`,
//! `authorizedUserRepositories`, `usersWithPendingPermissions`, and `Repository.permissionsInfo`
//! / `Repository.authorizedUsers`; plus `repositoryAccessExplanation` ("why can X see Y" for the
//! permissions admin page). Site admins only.
//!
//! Granting needs `permissions.userMapping.enabled`; users are named by
//! `permissions.userMapping.bindID` (a verified email or a username). Names that match no user
//! yet are kept as pending permissions and granted when that user is created or verifies the
//! email.

use async_graphql::{Context, Enum, ID, InputObject, Object, Result, SimpleObject};
use chrono::{DateTime, Utc};
use serde_json::json;
use sourcegull_authn::AuthVia;
use sourcegull_config::BindId;
use sourcegull_db::{BindKind, ListReposOptions, PermsSource, RepoOrder, RepoPermsInfo};
use sourcegull_types::{ExternalServiceKind, RepoAccess, RepoId, UserId};

use crate::EmptyResponse;
use crate::audit::{self, events};
use crate::context::{
    codes, db_error, error, page_size, require_site_admin, services, to_int, validation,
};
use crate::pagination::{PageInfo, decode_cursor};
use crate::repos::{RepositoryConnection, RepositoryNode, parse_repository_id};
use crate::users::{UserConnection, UserNode, parse_user_id};

/// Most users one `setRepositoryPermissionsForUsers` call may name.
const MAX_USERS_PER_CALL: usize = 10_000;

/// A permission on a repository.
#[derive(Enum, Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum RepositoryPermission {
    /// Read the repository's code and metadata.
    #[default]
    Read,
}

/// A user and their permission, for `setRepositoryPermissionsForUsers`.
#[derive(InputObject)]
pub struct UserPermissionInput {
    /// The user's verified email or username, per `permissions.userMapping.bindID`.
    #[graphql(name = "bindID")]
    pub bind_id: String,
    /// The permission to grant.
    #[graphql(default)]
    pub permission: RepositoryPermission,
}

/// A repository's permission state.
#[derive(SimpleObject)]
pub struct PermissionsInfo {
    /// The permissions granted.
    pub permissions: Vec<RepositoryPermission>,
    /// When code host permissions were last synced.
    pub synced_at: Option<DateTime<Utc>>,
    /// When the permissions last changed.
    pub updated_at: Option<DateTime<Utc>>,
    /// Where the permissions come from; null for this API's rows.
    pub source: Option<String>,
    /// Whether every user may read the repository.
    pub unrestricted: bool,
}

impl From<RepoPermsInfo> for PermissionsInfo {
    fn from(i: RepoPermsInfo) -> Self {
        Self {
            permissions: vec![RepositoryPermission::Read],
            synced_at: i.synced_at,
            updated_at: Some(i.updated_at),
            source: None,
            unrestricted: i.unrestricted,
        }
    }
}

/// Why a user can read a repository.
#[derive(Enum, Copy, Clone, Debug, PartialEq, Eq)]
pub enum RepositoryAccessReasonKind {
    /// Site admins read everything (`authz.enforceForSiteAdmins` is off).
    SiteAdmin,
    /// The repository is public on the code host.
    Public,
    /// Marked unrestricted with `setRepositoryPermissionsUnrestricted`.
    Unrestricted,
    /// Listed by a code host connection without an `authorization` block.
    UnrestrictedConnection,
    /// Granted with `setRepositoryPermissionsForUsers`.
    ExplicitApi,
    /// Granted by the code host permissions sync.
    PermissionsSync,
}

/// One reason a user can read a repository.
#[derive(SimpleObject)]
pub struct RepositoryAccessReason {
    pub kind: RepositoryAccessReasonKind,
    /// A sentence for people.
    pub detail: String,
    /// When the grant was recorded (grants only).
    pub granted_at: Option<DateTime<Utc>>,
}

/// Whether and why a user can read a repository.
#[derive(SimpleObject)]
pub struct RepositoryAccessExplanation {
    /// Whether the user can read the repository now, as every read path decides it. It can lag
    /// the reasons by the checker's cache lifetime (seconds) when a change notification is lost.
    pub can_read: bool,
    /// Every reason that applies; empty when the user cannot read the repository.
    pub reasons: Vec<RepositoryAccessReason>,
    /// A summary: the reasons, or why there is none.
    pub summary: String,
    /// When the user's code host permissions were last synced.
    pub user_synced_at: Option<DateTime<Utc>>,
    /// When the repository's code host permissions were last synced.
    pub repository_synced_at: Option<DateTime<Utc>>,
}

fn bind_kind(ctx: &Context<'_>) -> Result<BindKind> {
    Ok(
        match services(ctx)?
            .config
            .load()
            .value
            .permissions_user_mapping
            .bind_id
        {
            BindId::Email => BindKind::Email,
            BindId::Username => BindKind::Username,
        },
    )
}

fn require_user_mapping(ctx: &Context<'_>) -> Result<()> {
    if services(ctx)?
        .config
        .load()
        .value
        .permissions_user_mapping
        .enabled
    {
        Ok(())
    } else {
        Err(error(
            codes::FORBIDDEN,
            "Explicit permissions are disabled: set \"permissions.userMapping\": \
             {\"enabled\": true} in the site configuration.",
        ))
    }
}

/// The live user named `bind_id` under `kind`.
async fn find_user(ctx: &Context<'_>, kind: BindKind, bind_id: &str) -> Result<Option<UserId>> {
    let db = &services(ctx)?.db;
    Ok(match kind {
        BindKind::Username => db
            .users()
            .get_by_username(bind_id)
            .await
            .map_err(db_error)?
            .map(|u| u.id),
        BindKind::Email => db
            .user_emails()
            .user_by_verified_email(bind_id)
            .await
            .map_err(db_error)?,
    })
}

/// A page of the repositories with `ids`, by name.
async fn repository_page(
    ctx: &Context<'_>,
    ids: Vec<RepoId>,
    first: i32,
    after: Option<String>,
) -> Result<RepositoryConnection> {
    let db = &services(ctx)?.db;
    let limit = page_size(Some(first), 50, 1000);
    let offset = decode_cursor(after.as_deref())?;
    let opts = ListReposOptions {
        ids: Some(ids),
        order: RepoOrder::Name,
        limit: limit + 1,
        offset,
        ..ListReposOptions::default()
    };
    // Site admins only (callers check).
    #[allow(clippy::disallowed_methods)]
    let repos = db.repos().list(&opts).await.map_err(db_error)?;
    // Site admins only (callers check).
    #[allow(clippy::disallowed_methods)]
    let total = db.repos().count(&opts).await.map_err(db_error)?;
    let page_info = PageInfo::new(offset, limit, repos.len());
    Ok(RepositoryConnection {
        nodes: repos
            .into_iter()
            .take(usize::try_from(limit).unwrap_or(0))
            .map(|repo| RepositoryNode {
                repo,
                gitserver: None,
            })
            .collect(),
        total_count: to_int(total),
        page_info,
    })
}

/// `Repository.permissionsInfo` (site admins only).
pub(crate) async fn repository_permissions_info(
    ctx: &Context<'_>,
    repo: RepoId,
) -> Result<Option<PermissionsInfo>> {
    require_site_admin(ctx)?;
    let info = services(ctx)?
        .db
        .perms()
        .repo_info(repo)
        .await
        .map_err(db_error)?;
    Ok(info.map(PermissionsInfo::from))
}

/// `Repository.authorizedUsers` (site admins only): the users granted the repository. An
/// unrestricted repository is readable by everyone without being listed here.
pub(crate) async fn repository_authorized_users(
    ctx: &Context<'_>,
    repo: RepoId,
    first: i32,
    after: Option<String>,
) -> Result<UserConnection> {
    require_site_admin(ctx)?;
    let db = &services(ctx)?.db;
    let limit = page_size(Some(first), 50, 1000);
    let offset = decode_cursor(after.as_deref())?;
    let (ids, total) = db
        .perms()
        .repo_users(repo, limit, offset)
        .await
        .map_err(db_error)?;
    let mut nodes = Vec::with_capacity(ids.len());
    for id in ids {
        if let Some(u) = db.users().get_by_id(id).await.map_err(db_error)? {
            nodes.push(UserNode(u));
        }
    }
    Ok(UserConnection {
        nodes,
        total_count: to_int(total),
    })
}

/// Permission queries.
#[derive(Default)]
pub struct PermissionsQuery;

#[Object]
impl PermissionsQuery {
    /// The repositories granted to the user with `username` or `email` (site admins only).
    /// For a user who does not exist yet, the repositories pending for that name under
    /// `permissions.userMapping.bindID`.
    async fn authorized_user_repositories(
        &self,
        ctx: &Context<'_>,
        username: Option<String>,
        email: Option<String>,
        #[graphql(default)] perm: RepositoryPermission,
        first: i32,
        after: Option<String>,
    ) -> Result<RepositoryConnection> {
        let _ = perm;
        require_site_admin(ctx)?;
        let db = &services(ctx)?.db;
        let (kind, name) = match (username, email) {
            (Some(u), None) => (BindKind::Username, u),
            (None, Some(e)) => (BindKind::Email, e),
            _ => {
                return Err(error(
                    codes::INVALID_INPUT,
                    "Pass exactly one of username and email.",
                ));
            }
        };
        let ids = match find_user(ctx, kind, &name).await? {
            Some(user) => db.perms().user_repos(user).await.map_err(db_error)?,
            None if kind == bind_kind(ctx)? => db
                .perms()
                .pending_repos(kind, &name)
                .await
                .map_err(db_error)?,
            None => Vec::new(),
        };
        repository_page(ctx, ids, first, after).await
    }

    /// Whether and why the user can read the repository (site admins only): "why can X see Y".
    async fn repository_access_explanation(
        &self,
        ctx: &Context<'_>,
        user: ID,
        repository: ID,
    ) -> Result<Option<RepositoryAccessExplanation>> {
        require_site_admin(ctx)?;
        let svc = services(ctx)?;
        let (user, repo) = (parse_user_id(&user)?, parse_repository_id(&repository)?);
        let Some(actor) = svc
            .auth
            .actor_for(user, AuthVia::Session)
            .await
            .map_err(|e| error(codes::INTERNAL, e.to_string()))?
        else {
            return Ok(None);
        };
        let Some(facts) = svc
            .db
            .perms()
            .access_facts(user, repo)
            .await
            .map_err(db_error)?
        else {
            return Ok(None);
        };
        let config = svc.config.load();
        let rules = &config.value;
        let user_mapping = rules.permissions_user_mapping.enabled;
        let reason = |kind, detail: String| RepositoryAccessReason {
            kind,
            detail,
            granted_at: None,
        };
        let host = facts
            .service_type
            .parse::<ExternalServiceKind>()
            .map_or("the code host", ExternalServiceKind::name);
        let mut reasons = Vec::new();
        if actor.site_admin && !rules.authz_enforce_for_site_admins {
            reasons.push(reason(
                RepositoryAccessReasonKind::SiteAdmin,
                "Site admins can read every repository (authz.enforceForSiteAdmins is off).".into(),
            ));
        }
        if !facts.private {
            reasons.push(reason(
                RepositoryAccessReasonKind::Public,
                format!("The repository is public on {host}."),
            ));
        }
        if facts.private && facts.marked_unrestricted {
            reasons.push(reason(
                RepositoryAccessReasonKind::Unrestricted,
                "The repository was marked unrestricted with the permissions API.".into(),
            ));
        }
        if facts.private && !facts.marked_unrestricted && !user_mapping {
            for (id, name) in &facts.unrestricted_connections {
                reasons.push(reason(
                    RepositoryAccessReasonKind::UnrestrictedConnection,
                    format!(
                        "The code host connection {name:?} (#{id}) lists it and has no \
                         \"authorization\" block."
                    ),
                ));
            }
        }
        for (source, at) in &facts.grants {
            let (kind, detail) = if source == PermsSource::Explicit.as_str() {
                (
                    RepositoryAccessReasonKind::ExplicitApi,
                    "Granted with setRepositoryPermissionsForUsers.".to_owned(),
                )
            } else {
                (
                    RepositoryAccessReasonKind::PermissionsSync,
                    format!("Granted by the permissions sync from {host}."),
                )
            };
            reasons.push(RepositoryAccessReason {
                kind,
                detail,
                granted_at: Some(*at),
            });
        }
        let can_read = svc
            .authz
            .access(Some(&actor))
            .await
            .map_err(db_error)?
            .can_read(repo);
        let summary = if !reasons.is_empty() {
            reasons
                .iter()
                .map(|r| r.detail.as_str())
                .collect::<Vec<_>>()
                .join(" ")
        } else if user_mapping && !facts.unrestricted_connections.is_empty() {
            "The repository is private and not granted to the user. Connections without an \
             \"authorization\" block do not unrestrict repositories while \
             permissions.userMapping is enabled."
                .into()
        } else {
            "The repository is private and not granted to the user: neither the code host \
             permissions sync nor the permissions API gave them access."
                .into()
        };
        let user_synced_at = svc
            .db
            .perms_sync_jobs()
            .user_synced_at(user)
            .await
            .map_err(db_error)?;
        Ok(Some(RepositoryAccessExplanation {
            can_read,
            reasons,
            summary,
            user_synced_at,
            repository_synced_at: facts.repo_synced_at,
        }))
    }

    /// Names (per `permissions.userMapping.bindID`) with pending permissions, i.e. not
    /// matching any user yet (site admins only).
    async fn users_with_pending_permissions(&self, ctx: &Context<'_>) -> Result<Vec<String>> {
        require_site_admin(ctx)?;
        services(ctx)?
            .db
            .perms()
            .pending_bind_ids(bind_kind(ctx)?)
            .await
            .map_err(db_error)
    }
}

/// Permission mutations.
#[derive(Default)]
pub struct PermissionsMutation;

#[Object]
impl PermissionsMutation {
    /// Replaces the users who may read `repository` with `userPermissions` (site admins only;
    /// needs `permissions.userMapping.enabled`). Names matching no user yet are kept pending.
    async fn set_repository_permissions_for_users(
        &self,
        ctx: &Context<'_>,
        repository: ID,
        user_permissions: Vec<UserPermissionInput>,
    ) -> Result<EmptyResponse> {
        let actor = require_site_admin(ctx)?;
        require_user_mapping(ctx)?;
        if user_permissions.len() > MAX_USERS_PER_CALL {
            return Err(validation(vec![format!(
                "At most {MAX_USERS_PER_CALL} users per call."
            )]));
        }
        let svc = services(ctx)?;
        let repo = parse_repository_id(&repository)?;
        if svc
            .db
            .repos()
            .get_authorized(repo, &RepoAccess::All)
            .await
            .map_err(db_error)?
            .is_none()
        {
            return Err(error(codes::NOT_FOUND, "Repository not found."));
        }
        let kind = bind_kind(ctx)?;
        let mut users = Vec::new();
        let mut pending = Vec::new();
        for p in &user_permissions {
            let name = p.bind_id.trim();
            if name.is_empty() {
                return Err(validation(vec!["A bindID must not be empty.".to_owned()]));
            }
            match find_user(ctx, kind, name).await? {
                Some(u) => users.push(u),
                None => pending.push(name.to_owned()),
            }
        }
        svc.db
            .perms()
            .set_repo_perms(repo, PermsSource::Explicit, &users, Some((kind, &pending)))
            .await
            .map_err(db_error)?;
        audit::record(
            svc,
            events::REPO_PERMISSIONS_SET,
            Some(actor.user_id),
            json!({ "repo": repo, "users": users.len(), "pending": pending.len() }),
        )
        .await;
        Ok(EmptyResponse)
    }

    /// Marks repositories readable by every user, or restores their permissions (site admins
    /// only).
    async fn set_repository_permissions_unrestricted(
        &self,
        ctx: &Context<'_>,
        repositories: Vec<ID>,
        unrestricted: bool,
    ) -> Result<EmptyResponse> {
        let actor = require_site_admin(ctx)?;
        let svc = services(ctx)?;
        let ids = repositories
            .iter()
            .map(|id| parse_repository_id(id))
            .collect::<Result<Vec<_>>>()?;
        let live = svc
            .db
            .repos()
            .readable_ids(&ids, &RepoAccess::All)
            .await
            .map_err(db_error)?;
        if let Some(missing) = ids.iter().find(|id| !live.contains(id)) {
            return Err(error(
                codes::NOT_FOUND,
                format!("Repository {} not found.", missing.get()),
            ));
        }
        svc.db
            .perms()
            .set_unrestricted(&live, unrestricted)
            .await
            .map_err(db_error)?;
        audit::record(
            svc,
            events::REPO_PERMISSIONS_UNRESTRICTED,
            Some(actor.user_id),
            json!({ "repos": live, "unrestricted": unrestricted }),
        )
        .await;
        Ok(EmptyResponse)
    }
}
