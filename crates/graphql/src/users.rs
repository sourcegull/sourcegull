//! Users: `currentUser`, `users`, the `User` type and `setUserIsSiteAdmin`.

use std::borrow::Cow;

use async_graphql::parser::types::Field;
use async_graphql::registry::Registry;
use async_graphql::{
    Context, ContextSelectionSet, ID, Interface, Object, OutputType, Positioned, Result,
    ServerResult, SimpleObject, Value,
};
use chrono::{DateTime, Utc};
use serde_json::json;
use sourcegull_db::ListUsersOptions;
use sourcegull_services::relay::marshal_id;
use sourcegull_types::{User, UserEmail, UserId};

use crate::EmptyResponse;
use crate::account::{require_self_or_site_admin, username_change_allowed};
use crate::audit::{self, events};
use crate::context::{
    auth_error, codes, db_error, error, page_size, require_site_admin, require_viewer, services,
    to_int, viewer,
};
use crate::oauth_grants::OAuthGrantNode;
use crate::orgs::{OrgConnection, OrgNode};
use crate::tokens::AccessTokenConnection;

pub use sourcegull_graphql_core::ids::{USER_KIND, parse_user_id};

/// Something that can be a team member (only users).
// The interface's field takes arguments, declared with `arg(...)`; clippy mistakes the
// repeated `ty = "Option<String>"` for duplicate attributes.
#[allow(clippy::duplicated_attributes)]
#[derive(Interface)]
#[graphql(field(
    name = "teams",
    ty = "UserTeams",
    desc = "The teams this member is a direct member of.",
    arg(name = "first", ty = "Option<i32>"),
    arg(name = "after", ty = "Option<String>"),
    arg(name = "search", ty = "Option<String>")
))]
pub enum TeamMember {
    User(UserNode),
}

/// The `TeamConnection` of `User.teams`. Registering it registers the `TeamMember` interface
/// first, while `User` is being registered, so `User` lists `TeamMember` as its first
/// interface.
pub struct UserTeams(sourcegull_own_graphql::TeamConnection);

impl OutputType for UserTeams {
    fn type_name() -> Cow<'static, str> {
        <sourcegull_own_graphql::TeamConnection as OutputType>::type_name()
    }

    fn create_type_info(registry: &mut Registry) -> String {
        <TeamMember as OutputType>::create_type_info(registry);
        <sourcegull_own_graphql::TeamConnection as OutputType>::create_type_info(registry)
    }

    async fn resolve(
        &self,
        ctx: &ContextSelectionSet<'_>,
        field: &Positioned<Field>,
    ) -> ServerResult<Value> {
        OutputType::resolve(&self.0, ctx, field).await
    }
}

/// Loads a live user or fails with `NOT_FOUND`.
pub async fn load_user(ctx: &Context<'_>, id: UserId) -> Result<User> {
    services(ctx)?
        .db
        .users()
        .get_by_id(id)
        .await
        .map_err(db_error)?
        .ok_or_else(|| error(codes::NOT_FOUND, "User not found."))
}

/// A user account.
#[derive(Debug, Clone)]
pub struct UserNode(pub User);

#[Object(name = "User")]
impl UserNode {
    /// The opaque node ID.
    pub async fn id(&self) -> ID {
        ID(marshal_id(USER_KIND, self.0.id.get()))
    }

    /// The unique username.
    async fn username(&self) -> &str {
        &self.0.username
    }

    /// The user's page (`/users/<username>`; bulk change URLs build on
    /// it).
    pub async fn url(&self) -> String {
        format!("/users/{}", self.0.username)
    }

    /// The display name, if set.
    async fn display_name(&self) -> Option<&str> {
        self.0.display_name.as_deref()
    }

    /// The avatar URL, if set.
    #[graphql(name = "avatarURL")]
    async fn avatar_url(&self) -> Option<&str> {
        self.0.avatar_url.as_deref()
    }

    /// Whether the user is a site admin.
    async fn site_admin(&self) -> bool {
        self.0.site_admin
    }

    /// Whether an identity provider manages the user through SCIM: its username,
    /// emails and active state are then changed there, not in Sourcegull.
    #[graphql(name = "scimControlled")]
    async fn scim_controlled(&self, ctx: &Context<'_>) -> Result<bool> {
        services(ctx)?
            .db
            .scim()
            .is_controlled(self.0.id)
            .await
            .map_err(db_error)
    }

    /// When the account was created.
    async fn created_at(&self) -> DateTime<Utc> {
        self.0.created_at
    }

    /// When the account was last updated.
    async fn updated_at(&self) -> DateTime<Utc> {
        self.0.updated_at
    }

    /// When a site admin or SCIM deactivated the account, or null for an active account.
    async fn deactivated_at(&self) -> Option<DateTime<Utc>> {
        self.0.deleted_at
    }

    /// Whether the user has a built-in password (the user themself or site admins only).
    async fn has_password(&self, ctx: &Context<'_>) -> Result<bool> {
        require_self_or_site_admin(ctx, self.0.id)?;
        Ok(services(ctx)?
            .db
            .users()
            .password_hash(self.0.id)
            .await
            .map_err(db_error)?
            .is_some())
    }

    /// Whether the viewer may change the user's username: site admins, or the user themself when
    /// `auth.enableUsernameChanges` is on; never for SCIM-managed users.
    async fn viewer_can_change_username(&self, ctx: &Context<'_>) -> Result<bool> {
        let Some(actor) = viewer(ctx) else {
            return Ok(false);
        };
        username_change_allowed(ctx, actor, self.0.id).await
    }

    /// The teams the user is a direct member of.
    pub async fn teams(
        &self,
        ctx: &Context<'_>,
        first: Option<i32>,
        after: Option<String>,
        search: Option<String>,
    ) -> Result<UserTeams> {
        sourcegull_own_graphql::teams::user_teams(ctx, self.0.id, first, after, search)
            .await
            .map(UserTeams)
    }

    /// The user's email addresses (the user themself or site admins only).
    async fn emails(&self, ctx: &Context<'_>) -> Result<Vec<UserEmailNode>> {
        let actor = require_viewer(ctx)?;
        if !actor.is_self_or_site_admin(self.0.id) {
            return Err(error(
                codes::FORBIDDEN,
                "Only the user and site admins may see email addresses.",
            ));
        }
        let emails = services(ctx)?
            .db
            .user_emails()
            .list(self.0.id)
            .await
            .map_err(db_error)?;
        Ok(emails.into_iter().map(UserEmailNode).collect())
    }

    /// The user's access tokens (the user themself or site admins only).
    async fn access_tokens(
        &self,
        ctx: &Context<'_>,
        first: Option<i32>,
    ) -> Result<AccessTokenConnection> {
        let actor = require_viewer(ctx)?;
        let mut tokens = services(ctx)?
            .auth
            .list_access_tokens(actor, self.0.id)
            .await
            .map_err(auth_error)?;
        let total = tokens.len();
        if let Some(n) = first {
            tokens.truncate(usize::try_from(n.max(0)).unwrap_or(0));
        }
        Ok(AccessTokenConnection::new(tokens, total))
    }

    /// The apps (MCP clients) the user allowed to act as them through OAuth, newest first
    /// (the user themself or site admins only).
    async fn oauth_grants(&self, ctx: &Context<'_>) -> Result<Vec<OAuthGrantNode>> {
        let actor = require_viewer(ctx)?;
        let grants = services(ctx)?
            .auth
            .list_oauth_grants(actor, Some(self.0.id))
            .await
            .map_err(auth_error)?;
        Ok(grants.into_iter().map(OAuthGrantNode).collect())
    }

    /// The user's linked code host accounts (the user themself or site admins only).
    async fn external_accounts(
        &self,
        ctx: &Context<'_>,
    ) -> Result<crate::perms_sync::ExternalAccountConnection> {
        let actor = require_viewer(ctx)?;
        if !actor.is_self_or_site_admin(self.0.id) {
            return Err(error(
                codes::FORBIDDEN,
                "Only the user and site admins may see linked accounts.",
            ));
        }
        crate::perms_sync::user_external_accounts(ctx, self.0.id).await
    }

    /// Code hosts the user has to sign in with again so the permissions sync can see their
    /// private repositories there (the user themself or site admins only).
    async fn code_host_sign_ins_needed(
        &self,
        ctx: &Context<'_>,
    ) -> Result<Vec<crate::perms_sync::CodeHostSignIn>> {
        let actor = require_viewer(ctx)?;
        if !actor.is_self_or_site_admin(self.0.id) {
            return Err(error(
                codes::FORBIDDEN,
                "Only the user and site admins may see this.",
            ));
        }
        crate::perms_sync::user_code_host_sign_ins(ctx, self.0.id).await
    }

    /// When the user's code host permissions were last synced (site admins only).
    async fn permissions_info(
        &self,
        ctx: &Context<'_>,
    ) -> Result<crate::perms_sync::UserPermissionsInfo> {
        crate::perms_sync::user_permissions_info(ctx, self.0.id).await
    }

    /// Assigned roles (site admins only).
    async fn roles(
        &self,
        ctx: &Context<'_>,
        first: Option<i32>,
    ) -> Result<crate::rbac::RoleConnection> {
        crate::rbac::user_roles(ctx, self.0.id, first).await
    }

    /// Effective RBAC permissions of the signed-in user.
    async fn permissions(&self, ctx: &Context<'_>) -> Result<crate::rbac::PermissionConnection> {
        let actor = require_viewer(ctx)?;
        if actor.user_id != self.0.id {
            return Err(error(
                codes::FORBIDDEN,
                "Only the current user may see their permissions.",
            ));
        }
        crate::rbac::current_permissions(ctx).await
    }

    /// The executor secrets usable in the user's namespace: theirs and the global ones they do
    /// not overwrite (the user themself or site admins only).
    async fn executor_secrets(
        &self,
        ctx: &Context<'_>,
        scope: sourcegull_executors_graphql::executor_secrets::Scope,
        #[graphql(default = 50)] first: i32,
        after: Option<String>,
    ) -> Result<crate::namespace::NamespaceExecutorSecrets> {
        let ns = Some(sourcegull_db::Namespace::User(self.0.id));
        sourcegull_executors_graphql::executor_secrets::list(ctx, ns, scope, first, after)
            .await
            .map(crate::namespace::NamespaceExecutorSecrets)
    }

    /// The bulk changes in the user's namespace.
    async fn batch_changes(
        &self,
        ctx: &Context<'_>,
        #[graphql(default = 50)] first: Option<i32>,
        after: Option<String>,
        state: Option<crate::batches::BatchChangeStateGql>,
        states: Option<Vec<crate::batches::BatchChangeStateGql>>,
        viewer_can_administer: Option<bool>,
    ) -> Result<crate::batches::BatchChangeConnection> {
        let args = crate::batches::BatchChangesArgs {
            first,
            after,
            state,
            states,
            viewer_can_administer,
        };
        crate::batches::list_batch_changes(
            ctx,
            Some(sourcegull_db::Namespace::User(self.0.id)),
            args,
        )
        .await
    }

    /// The code hosts the user can publish changesets to, with the credential that would be
    /// used (theirs, else the site's; the user themself or site admins only).
    async fn batch_changes_code_hosts(
        &self,
        ctx: &Context<'_>,
        #[graphql(default = 50)] first: Option<i32>,
        after: Option<String>,
    ) -> Result<crate::batches::BatchChangesCodeHostConnection> {
        crate::batches::code_hosts(ctx, Some(self.0.id), first, after).await
    }

    /// The watches owned by the user or their organizations (the user themself
    /// or site admins only).
    async fn monitors(
        &self,
        ctx: &Context<'_>,
        #[graphql(default = 50)] first: i32,
        after: Option<String>,
    ) -> Result<sourcegull_monitors_graphql::MonitorConnection> {
        sourcegull_monitors_graphql::user_monitors(ctx, self.0.id, first, after).await
    }

    /// The user's latest settings (the user and site admins only).
    pub async fn latest_settings(
        &self,
        ctx: &Context<'_>,
    ) -> Result<Option<crate::settings::SettingsNode>> {
        crate::settings::latest(ctx, sourcegull_types::SettingsSubject::User(self.0.id)).await
    }

    /// The URL to the user's settings.
    #[graphql(name = "settingsURL")]
    pub async fn settings_url(&self) -> Option<String> {
        Some(format!("/users/{}/settings", self.0.username))
    }

    /// Whether the viewer may change the user's account and settings (the user and site
    /// admins).
    pub async fn viewer_can_administer(&self, ctx: &Context<'_>) -> bool {
        crate::context::viewer(ctx).is_some_and(|a| a.site_admin || a.user_id == self.0.id)
    }

    /// The settings cascade of the user: global, the user's organizations, the user (the
    /// user and site admins only).
    pub async fn settings_cascade(
        &self,
        ctx: &Context<'_>,
    ) -> Result<crate::settings::SettingsCascadeNode> {
        crate::settings::cascade(ctx, sourcegull_types::SettingsSubject::User(self.0.id)).await
    }

    /// Organizations the user is a member of.
    async fn organizations(&self, ctx: &Context<'_>) -> Result<OrgConnection> {
        require_viewer(ctx)?;
        let orgs = services(ctx)?
            .db
            .orgs()
            .list_for_user(self.0.id)
            .await
            .map_err(db_error)?;
        Ok(OrgConnection {
            total_count: to_int(i64::try_from(orgs.len()).unwrap_or(i64::MAX)),
            nodes: orgs.into_iter().map(OrgNode).collect(),
        })
    }
}

/// An email address of a user.
pub struct UserEmailNode(pub UserEmail);

#[Object(name = "UserEmail")]
impl UserEmailNode {
    /// The address.
    async fn email(&self) -> &str {
        &self.0.email
    }

    /// Whether it has been verified.
    async fn verified(&self) -> bool {
        self.0.is_verified()
    }

    /// Whether it is the user's primary address.
    async fn is_primary(&self) -> bool {
        self.0.is_primary
    }
}

/// A page of users.
#[derive(SimpleObject)]
pub struct UserConnection {
    /// The users on this page.
    pub nodes: Vec<UserNode>,
    /// All users matching the query.
    pub total_count: i32,
}

/// User queries.
#[derive(Default)]
pub struct UsersQuery;

#[Object]
impl UsersQuery {
    /// The signed-in user, or null for anonymous requests.
    async fn current_user(&self, ctx: &Context<'_>) -> Result<Option<UserNode>> {
        let Some(actor) = viewer(ctx) else {
            return Ok(None);
        };
        let user = services(ctx)?
            .db
            .users()
            .get_by_id(actor.user_id)
            .await
            .map_err(db_error)?;
        Ok(user.map(UserNode))
    }

    /// Looks up a user by username (signed-in users only).
    async fn user(&self, ctx: &Context<'_>, username: String) -> Result<Option<UserNode>> {
        require_viewer(ctx)?;
        let user = services(ctx)?
            .db
            .users()
            .get_by_username(&username)
            .await
            .map_err(db_error)?;
        Ok(user.map(UserNode))
    }

    /// Lists users, optionally filtered by a substring of the username or display name, with
    /// deactivated users when `includeDeactivated` is set (site admins only).
    async fn users(
        &self,
        ctx: &Context<'_>,
        first: Option<i32>,
        query: Option<String>,
        include_deactivated: Option<bool>,
    ) -> Result<UserConnection> {
        require_site_admin(ctx)?;
        let db = &services(ctx)?.db;
        let include_deactivated = include_deactivated.unwrap_or(false);
        let query = query.filter(|q| !q.trim().is_empty());
        let users = db
            .users()
            .list(&ListUsersOptions {
                query: query.clone(),
                include_deleted: include_deactivated,
                limit: page_size(first, 50, 1000),
                ..Default::default()
            })
            .await
            .map_err(db_error)?;
        let total = db
            .users()
            .count_matching(query.as_deref(), include_deactivated)
            .await
            .map_err(db_error)?;
        Ok(UserConnection {
            nodes: users.into_iter().map(UserNode).collect(),
            total_count: to_int(total),
        })
    }
}

/// User mutations.
#[derive(Default)]
pub struct UsersMutation;

#[derive(SimpleObject)]
pub struct RandomizeUserPasswordResult {
    #[graphql(name = "resetPasswordURL")]
    reset_password_url: String,
}

#[Object]
impl UsersMutation {
    /// Creates a single-use reset link for a user (site admins only).
    async fn randomize_user_password(
        &self,
        ctx: &Context<'_>,
        user: ID,
    ) -> Result<RandomizeUserPasswordResult> {
        require_site_admin(ctx)?;
        let svc = services(ctx)?;
        let id = parse_user_id(&user)?;
        load_user(ctx, id).await?;
        let email = svc
            .db
            .user_emails()
            .list(id)
            .await
            .map_err(db_error)?
            .into_iter()
            .find(|e| e.verified_at.is_some())
            .map(|e| e.email);
        let code = sourcegull_crypto::session::generate_session_id();
        let config = svc.config.load();
        svc.db
            .password_resets()
            .create(
                id,
                &sourcegull_crypto::session::hash_session_id(&code),
                config.value.auth_password_reset_link_expiry,
            )
            .await
            .map_err(db_error)?;
        let origin = config
            .value
            .external_url
            .as_deref()
            .unwrap_or("")
            .trim_end_matches('/');
        let url = format!(
            "{origin}/reset-password?userID={}&code={code}",
            marshal_id(USER_KIND, id.get())
        );
        if let (Some(app), Some(email)) = (&svc.app, email) {
            app.send_email(sourcegull_services::email::link_message(
                &email,
                "Reset your password",
                "Use this link to reset your password:",
                &url,
            ))
            .await;
        }
        Ok(RandomizeUserPasswordResult {
            reset_password_url: url,
        })
    }

    /// Sends a new verification link for an unverified user address.
    async fn resend_verification_email(
        &self,
        ctx: &Context<'_>,
        user: ID,
        email: String,
    ) -> Result<EmptyResponse> {
        let actor = require_viewer(ctx)?;
        let svc = services(ctx)?;
        let id = parse_user_id(&user)?;
        if actor.user_id != id && !actor.site_admin {
            return Err(error(
                codes::FORBIDDEN,
                "Cannot resend another user's verification email.",
            ));
        }
        load_user(ctx, id).await?;
        let app = svc
            .app
            .as_ref()
            .ok_or_else(|| error(codes::INTERNAL, "Application state unavailable."))?;
        sourcegull_services::email::send_verification(app, id, &email)
            .await
            .map_err(db_error)?;
        Ok(EmptyResponse)
    }

    /// Sets or revokes address verification (site admins only).
    async fn set_user_email_verified(
        &self,
        ctx: &Context<'_>,
        user: ID,
        email: String,
        verified: bool,
    ) -> Result<EmptyResponse> {
        let actor = require_site_admin(ctx)?;
        let svc = services(ctx)?;
        let id = parse_user_id(&user)?;
        crate::declared::ensure_email_verification_allowed(svc, &email).await?;
        if verified {
            svc.db
                .user_emails()
                .verify(id, &email)
                .await
                .map_err(db_error)?;
        } else {
            svc.db
                .user_emails()
                .unverify(id, &email)
                .await
                .map_err(db_error)?;
        }
        audit::record(
            svc,
            events::ROLE_CHANGE_GRANTED,
            Some(actor.user_id),
            json!({ "userID": id, "email": email, "verified": verified }),
        )
        .await;
        Ok(EmptyResponse)
    }

    /// Grants or revokes site admin (site admins only). The last site admin cannot be demoted.
    #[graphql(name = "setUserIsSiteAdmin")]
    async fn set_user_is_site_admin(
        &self,
        ctx: &Context<'_>,
        #[graphql(name = "userID")] user_id: ID,
        site_admin: bool,
    ) -> Result<EmptyResponse> {
        let actor = require_site_admin(ctx)?;
        let svc = services(ctx)?;
        let id = parse_user_id(&user_id)?;
        let user = load_user(ctx, id).await?;
        if user.site_admin == site_admin {
            return Ok(EmptyResponse);
        }
        crate::declared::ensure_site_admin_change_allowed(svc, id, site_admin).await?;
        let users = svc.db.users();
        let last_admin_error = || {
            error(
                codes::FORBIDDEN,
                "Cannot revoke site admin from the last site admin.",
            )
        };
        if !site_admin && users.count_site_admins().await.map_err(db_error)? <= 1 {
            return Err(last_admin_error());
        }
        users
            .set_site_admin(id, site_admin)
            .await
            .map_err(db_error)?;
        // Concurrent demotions can pass the check above together; re-check after the write and
        // restore this user if no site admin is left.
        if !site_admin && users.count_site_admins().await.map_err(db_error)? == 0 {
            users.set_site_admin(id, true).await.map_err(db_error)?;
            svc.auth.invalidate_user(id).await;
            return Err(last_admin_error());
        }
        svc.auth.invalidate_user(id).await;
        audit::record(
            svc,
            if site_admin {
                events::ROLE_CHANGE_GRANTED
            } else {
                events::ROLE_CHANGE_DENIED
            },
            Some(actor.user_id),
            json!({ "userID": id, "siteAdmin": site_admin }),
        )
        .await;
        Ok(EmptyResponse)
    }
}
