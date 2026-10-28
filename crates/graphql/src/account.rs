//! Account self-service and site admin account actions: `updateUser`, `updatePassword`, the
//! email mutations, and `deactivateUser`/`reactivateUser`.
//!
//! A user changes their own profile, password and emails; site admins may change any user's
//! profile and emails. Usernames, emails and the active state of SCIM-managed users change only
//! through SCIM.

use async_graphql::{Context, ID, Object, Result};
use serde_json::json;
use sourcegull_authn::{Actor, SessionToken};
use sourcegull_db::{DbError, ProfileUpdate};
use sourcegull_types::UserId;

use crate::EmptyResponse;
use crate::audit::{self, events};
use crate::context::{
    auth_error, codes, db_error, error, require_site_admin, require_viewer, services,
};
use crate::users::{UserNode, load_user, parse_user_id};

/// The session cookie the GraphQL request carried, if any. `updatePassword` keeps this session
/// and ends the user's others.
#[derive(Debug, Clone, Default)]
pub struct CurrentSession(pub Option<SessionToken>);

const MAX_DISPLAY_NAME_LEN: usize = 255;
const MAX_AVATAR_URL_LEN: usize = 2048;

/// The viewer, if they are `user` or a site admin.
pub(crate) fn require_self_or_site_admin<'a>(ctx: &Context<'a>, user: UserId) -> Result<&'a Actor> {
    let actor = require_viewer(ctx)?;
    if !actor.is_self_or_site_admin(user) {
        return Err(error(
            codes::FORBIDDEN,
            "Only the user and site admins may do this.",
        ));
    }
    Ok(actor)
}

/// Whether `actor` may change `user`'s username.
pub(crate) async fn username_change_allowed(
    ctx: &Context<'_>,
    actor: &Actor,
    user: UserId,
) -> Result<bool> {
    let svc = services(ctx)?;
    let allowed = actor.site_admin
        || (actor.user_id == user && svc.config.load().value.auth_enable_username_changes);
    Ok(allowed && !scim_controlled(ctx, user).await?)
}

async fn scim_controlled(ctx: &Context<'_>, user: UserId) -> Result<bool> {
    services(ctx)?
        .db
        .scim()
        .is_controlled(user)
        .await
        .map_err(db_error)
}

/// Fails with `FORBIDDEN` when an identity provider manages `user` through SCIM.
async fn ensure_not_scim(ctx: &Context<'_>, user: UserId, what: &str) -> Result<()> {
    if scim_controlled(ctx, user).await? {
        return Err(error(
            codes::FORBIDDEN,
            format!("Your identity provider manages this user through SCIM; change {what} there."),
        ));
    }
    Ok(())
}

fn invalid(message: impl Into<String>) -> async_graphql::Error {
    error(codes::INVALID_INPUT, message)
}

/// `Some(None)` for an empty value (clears the field), `Some(Some(v))` otherwise.
fn optional_text(value: Option<String>) -> Option<Option<String>> {
    value.map(|v| {
        let v = v.trim().to_owned();
        (!v.is_empty()).then_some(v)
    })
}

fn email_error(e: DbError) -> async_graphql::Error {
    match e {
        DbError::Invalid(msg) => invalid(capitalize(msg)),
        DbError::AlreadyExists { .. } => {
            error(codes::ALREADY_EXISTS, "The user already has this email.")
        }
        other => db_error(other),
    }
}

fn capitalize(msg: &str) -> String {
    let mut chars = msg.chars();
    match chars.next() {
        Some(c) => format!("{}{}.", c.to_uppercase(), chars.as_str()),
        None => String::new(),
    }
}

/// Account mutations.
#[derive(Default)]
pub struct AccountMutation;

#[Object]
impl AccountMutation {
    /// Changes a user's profile (the user themself or site admins). Omitted or null fields stay
    /// unchanged; an empty `displayName` or `avatarURL` clears it. `username` changes need
    /// `viewerCanChangeUsername`.
    async fn update_user(
        &self,
        ctx: &Context<'_>,
        user: ID,
        username: Option<String>,
        display_name: Option<String>,
        #[graphql(name = "avatarURL")] avatar_url: Option<String>,
    ) -> Result<UserNode> {
        let id = parse_user_id(&user)?;
        let actor = require_self_or_site_admin(ctx, id)?;
        let svc = services(ctx)?;
        let current = load_user(ctx, id).await?;
        let username = username
            .map(|u| u.trim().to_owned())
            .filter(|u| *u != current.username);
        if let Some(name) = &username {
            if !username_change_allowed(ctx, actor, id).await? {
                return Err(error(
                    codes::FORBIDDEN,
                    "Username changes are not allowed (auth.enableUsernameChanges).",
                ));
            }
            sourcegull_authn::validate::validate_username(name).map_err(auth_error)?;
        }
        let display_name = optional_text(display_name);
        if display_name
            .as_ref()
            .and_then(Option::as_ref)
            .is_some_and(|n| n.chars().count() > MAX_DISPLAY_NAME_LEN)
        {
            return Err(invalid(format!(
                "The display name is longer than {MAX_DISPLAY_NAME_LEN} characters."
            )));
        }
        let avatar_url = optional_text(avatar_url);
        if let Some(Some(url)) = &avatar_url {
            let ok = url.len() <= MAX_AVATAR_URL_LEN
                && url::Url::parse(url).is_ok_and(|u| matches!(u.scheme(), "http" | "https"));
            if !ok {
                return Err(invalid("The avatar URL must be an http or https URL."));
            }
        }
        let renamed = username.is_some();
        let updated = svc
            .db
            .users()
            .update_profile(
                id,
                &ProfileUpdate {
                    username,
                    display_name,
                    avatar_url,
                },
            )
            .await
            .map_err(|e| match e {
                DbError::AlreadyExists { .. } => {
                    error(codes::ALREADY_EXISTS, "The username is already taken.")
                }
                other => db_error(other),
            })?;
        if renamed {
            svc.auth.invalidate_user(id).await;
            svc.auth
                .log_event(
                    sourcegull_authn::events::USERNAME_CHANGED,
                    Some(id),
                    json!({ "from": current.username, "to": updated.username, "by": actor.user_id }),
                )
                .await;
        }
        Ok(UserNode(updated))
    }

    /// Changes the signed-in user's password (built-in sign-in only). `oldPassword` is required
    /// when the user has a password. Ends the user's other sessions.
    async fn update_password(
        &self,
        ctx: &Context<'_>,
        old_password: Option<String>,
        new_password: String,
    ) -> Result<EmptyResponse> {
        let actor = require_viewer(ctx)?;
        let svc = services(ctx)?;
        let keep = ctx.data_opt::<CurrentSession>().and_then(|s| s.0.as_ref());
        svc.auth
            .change_password(actor.user_id, old_password.as_deref(), &new_password, keep)
            .await
            .map_err(auth_error)?;
        Ok(EmptyResponse)
    }

    /// Adds an unverified email to a user (the user themself or site admins) and sends a
    /// verification link when email is configured. The first email becomes primary.
    async fn add_user_email(
        &self,
        ctx: &Context<'_>,
        user: ID,
        email: String,
    ) -> Result<EmptyResponse> {
        let id = parse_user_id(&user)?;
        require_self_or_site_admin(ctx, id)?;
        let svc = services(ctx)?;
        load_user(ctx, id).await?;
        ensure_not_scim(ctx, id, "emails").await?;
        let email = email.trim();
        sourcegull_authn::validate::validate_email(email).map_err(auth_error)?;
        svc.db
            .user_emails()
            .add(id, email)
            .await
            .map_err(email_error)?;
        if let Some(app) = &svc.app
            && let Err(e) = sourcegull_services::email::send_verification(app, id, email).await
        {
            tracing::warn!(error = %e, "sending the verification email failed");
        }
        Ok(EmptyResponse)
    }

    /// Removes an email from a user (the user themself or site admins). The primary email and
    /// the only email cannot be removed.
    async fn remove_user_email(
        &self,
        ctx: &Context<'_>,
        user: ID,
        email: String,
    ) -> Result<EmptyResponse> {
        let id = parse_user_id(&user)?;
        require_self_or_site_admin(ctx, id)?;
        let svc = services(ctx)?;
        load_user(ctx, id).await?;
        ensure_not_scim(ctx, id, "emails").await?;
        svc.db
            .user_emails()
            .remove(id, &email)
            .await
            .map_err(email_error)?;
        Ok(EmptyResponse)
    }

    /// Makes a verified email the user's primary one (the user themself or site admins).
    async fn set_user_email_primary(
        &self,
        ctx: &Context<'_>,
        user: ID,
        email: String,
    ) -> Result<EmptyResponse> {
        let id = parse_user_id(&user)?;
        require_self_or_site_admin(ctx, id)?;
        let svc = services(ctx)?;
        load_user(ctx, id).await?;
        ensure_not_scim(ctx, id, "emails").await?;
        svc.db
            .user_emails()
            .set_primary(id, &email)
            .await
            .map_err(email_error)?;
        Ok(EmptyResponse)
    }

    /// Deactivates a user (site admins only): they cannot sign in, their sessions end and their
    /// access tokens and app grants are revoked. Site admins cannot deactivate themselves.
    async fn deactivate_user(&self, ctx: &Context<'_>, user: ID) -> Result<EmptyResponse> {
        let actor = require_site_admin(ctx)?;
        let svc = services(ctx)?;
        let id = parse_user_id(&user)?;
        if id == actor.user_id {
            return Err(error(codes::FORBIDDEN, "You cannot deactivate yourself."));
        }
        let target = load_user(ctx, id).await?;
        ensure_not_scim(ctx, id, "its active state").await?;
        svc.db.users().soft_delete(id).await.map_err(db_error)?;
        if let Err(e) = svc.auth.end_user_sessions(id).await {
            tracing::warn!(error = %e, user = id.get(), "ending sessions of a deactivated user failed");
        }
        audit::record(
            svc,
            events::USER_DEACTIVATED,
            Some(actor.user_id),
            json!({ "userID": id, "username": target.username }),
        )
        .await;
        Ok(EmptyResponse)
    }

    /// Reactivates a deactivated user (site admins only). Revoked access tokens stay revoked.
    async fn reactivate_user(&self, ctx: &Context<'_>, user: ID) -> Result<UserNode> {
        let actor = require_site_admin(ctx)?;
        let svc = services(ctx)?;
        let id = parse_user_id(&user)?;
        let target = svc
            .db
            .users()
            .get_by_id_including_deleted(id)
            .await
            .map_err(db_error)?
            .ok_or_else(|| error(codes::NOT_FOUND, "User not found."))?;
        if target.deleted_at.is_none() {
            return Ok(UserNode(target));
        }
        ensure_not_scim(ctx, id, "its active state").await?;
        let user = svc.db.users().reactivate(id).await.map_err(|e| match e {
            DbError::AlreadyExists { .. } => error(
                codes::ALREADY_EXISTS,
                "Another user has this username now; rename that user first.",
            ),
            other => db_error(other),
        })?;
        svc.auth.invalidate_user(id).await;
        audit::record(
            svc,
            events::USER_REACTIVATED,
            Some(actor.user_id),
            json!({ "userID": id, "username": user.username }),
        )
        .await;
        Ok(UserNode(user))
    }
}
