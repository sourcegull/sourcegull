//! Organizations: the `Org` type, members, invitations and org settings:
//! `organization(name)`, `organizations`, `createOrganization`,
//! `updateOrganization`, `deleteOrganization`, `addUserToOrganization`,
//! `removeUserFromOrganization`, `inviteUserToOrganization`,
//! `respondToOrganizationInvitation`, `invitationByToken`, `pendingInvitations`,
//! `revokeOrganizationInvitation` and `resendOrganizationInvitationNotification`.
//!
//! Permissions: any signed-in user may create an organization (and
//! becomes its first member); every member administers it (update, invite, remove members,
//! settings); listing all organizations, deleting one and adding a user without an invitation
//! are for site admins. Members, settings and invitations of an organization are visible to
//! its members and site admins only. Invitations are answered by their recipient: the invited
//! user, or for an email invitation a user with that verified address.
//!
//! An invitation carries a link token (`/organizations/invitation/<token>`); only its SHA-256
//! is stored. Invitations expire after [`INVITATION_EXPIRY`].

use async_graphql::{Context, Enum, ID, Object, Result, SimpleObject};
use chrono::{DateTime, Utc};
use serde_json::json;
use sourcegull_db::{NewOrgInvitation, OrgInvitation};
use sourcegull_services::relay::{marshal_id, unmarshal_int_id};
use sourcegull_types::{Org, OrgId, OrgInvitationId, OrgRole, SettingsSubject, UserId};

use crate::EmptyResponse;
use crate::audit::{self, events};
use crate::context::{
    auth_error, codes, db_error, error, page_size, require_site_admin, require_viewer, services,
    to_int, viewer,
};
use crate::pagination::{PageInfo, decode_cursor};
use crate::users::{UserNode, parse_user_id};

pub use sourcegull_graphql_core::ids::ORG_KIND;
/// Node kind of organization invitation IDs.
pub const ORG_INVITATION_KIND: &str = "OrgInvitation";
/// How long an invitation can be answered.
pub const INVITATION_EXPIRY: chrono::TimeDelta = chrono::TimeDelta::hours(48);

/// Parses an `Org` node ID.
pub fn parse_org_id(id: &str) -> Result<OrgId> {
    unmarshal_int_id(ORG_KIND, id).map(OrgId).ok_or_else(|| {
        error(
            codes::INVALID_INPUT,
            format!("Invalid organization ID {id:?}."),
        )
    })
}

fn parse_invitation_id(id: &str) -> Result<OrgInvitationId> {
    unmarshal_int_id(ORG_INVITATION_KIND, id)
        .map(OrgInvitationId)
        .ok_or_else(|| {
            error(
                codes::INVALID_INPUT,
                format!("Invalid invitation ID {id:?}."),
            )
        })
}

/// Whether the viewer is a member of `org` or a site admin.
async fn viewer_can_administer(ctx: &Context<'_>, org: OrgId) -> Result<bool> {
    match viewer(ctx) {
        Some(a) if a.site_admin => Ok(true),
        Some(a) => crate::namespace::is_member(ctx, org, a.user_id).await,
        None => Ok(false),
    }
}

async fn require_member_or_admin(ctx: &Context<'_>, org: OrgId) -> Result<()> {
    require_viewer(ctx)?;
    if viewer_can_administer(ctx, org).await? {
        return Ok(());
    }
    Err(error(
        codes::FORBIDDEN,
        "Only members of the organization and site admins may do this.",
    ))
}

/// Loads a live organization or fails with `NOT_FOUND`.
async fn load_org(ctx: &Context<'_>, id: OrgId) -> Result<Org> {
    services(ctx)?
        .db
        .orgs()
        .get(id)
        .await
        .map_err(db_error)?
        .ok_or_else(|| error(codes::NOT_FOUND, "Organization not found."))
}

/// An organization.
#[derive(Debug, Clone)]
pub struct OrgNode(pub Org);

#[Object(name = "Org")]
impl OrgNode {
    /// The opaque node ID.
    pub async fn id(&self) -> ID {
        ID(marshal_id(ORG_KIND, self.0.id.get()))
    }

    /// The unique name.
    async fn name(&self) -> &str {
        &self.0.name
    }

    /// The display name, if set.
    async fn display_name(&self) -> Option<&str> {
        self.0.display_name.as_deref()
    }

    /// The organization's page (`/organizations/<name>`).
    pub async fn url(&self) -> String {
        format!("/organizations/{}", self.0.name)
    }

    /// The URL to the organization's settings.
    #[graphql(name = "settingsURL")]
    pub async fn settings_url(&self) -> Option<String> {
        Some(format!("/organizations/{}/settings", self.0.name))
    }

    /// The name of the namespace (the organization's name).
    async fn namespace_name(&self) -> &str {
        &self.0.name
    }

    /// When the organization was created.
    async fn created_at(&self) -> DateTime<Utc> {
        self.0.created_at
    }

    /// The members who match `query` (username or display name), oldest membership first
    /// (members and site admins only).
    async fn members(
        &self,
        ctx: &Context<'_>,
        first: Option<i32>,
        after: Option<String>,
        query: Option<String>,
    ) -> Result<NewUsersConnection> {
        require_member_or_admin(ctx, self.0.id).await?;
        let db = &services(ctx)?.db;
        let orgs = db.orgs();
        let limit = page_size(first, 50, 1000);
        let offset = decode_cursor(after.as_deref())?;
        let query = query.as_deref().map(str::trim);
        let members = orgs
            .search_members(self.0.id, query, limit + 1, offset)
            .await
            .map_err(db_error)?;
        let total = orgs
            .count_members(self.0.id, query)
            .await
            .map_err(db_error)?;
        let page_info = PageInfo::new(offset, limit, members.len());
        let mut nodes = Vec::new();
        for m in members
            .into_iter()
            .take(usize::try_from(limit).unwrap_or(0))
        {
            if let Some(u) = db.users().get_by_id(m.user_id).await.map_err(db_error)? {
                nodes.push(UserNode(u));
            }
        }
        Ok(NewUsersConnection {
            nodes,
            total_count: to_int(total),
            page_info,
        })
    }

    /// Whether the viewer may administer the organization: every member may, and site
    /// admins.
    pub async fn viewer_can_administer(&self, ctx: &Context<'_>) -> Result<bool> {
        viewer_can_administer(ctx, self.0.id).await
    }

    /// Whether the viewer is a member.
    async fn viewer_is_member(&self, ctx: &Context<'_>) -> Result<bool> {
        match viewer(ctx) {
            Some(a) => crate::namespace::is_member(ctx, self.0.id, a.user_id).await,
            None => Ok(false),
        }
    }

    /// The viewer's pending invitation to join, if any.
    async fn viewer_pending_invitation(
        &self,
        ctx: &Context<'_>,
    ) -> Result<Option<OrgInvitationNode>> {
        let Some(actor) = viewer(ctx) else {
            return Ok(None);
        };
        let invitations = viewer_invitations(ctx, actor.user_id).await?;
        Ok(invitations
            .into_iter()
            .find(|i| i.org_id == self.0.id)
            .map(OrgInvitationNode))
    }

    /// The organization's latest settings (members and site admins only).
    pub async fn latest_settings(
        &self,
        ctx: &Context<'_>,
    ) -> Result<Option<crate::settings::SettingsNode>> {
        crate::settings::latest(ctx, SettingsSubject::Org(self.0.id)).await
    }

    /// The global and organization settings and their merge (members and site admins only).
    pub async fn settings_cascade(
        &self,
        ctx: &Context<'_>,
    ) -> Result<crate::settings::SettingsCascadeNode> {
        crate::settings::cascade(ctx, SettingsSubject::Org(self.0.id)).await
    }

    /// The executor secrets usable in the organization's namespace: its own and the global
    /// ones it does not overwrite (members and site admins only).
    async fn executor_secrets(
        &self,
        ctx: &Context<'_>,
        scope: sourcegull_executors_graphql::executor_secrets::Scope,
        #[graphql(default = 50)] first: i32,
        after: Option<String>,
    ) -> Result<crate::namespace::NamespaceExecutorSecrets> {
        let ns = Some(sourcegull_db::Namespace::Org(self.0.id));
        sourcegull_executors_graphql::executor_secrets::list(ctx, ns, scope, first, after)
            .await
            .map(crate::namespace::NamespaceExecutorSecrets)
    }

    /// The bulk changes in the organization's namespace.
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
            Some(sourcegull_db::Namespace::Org(self.0.id)),
            args,
        )
        .await
    }
}

/// A page of organizations.
#[derive(SimpleObject)]
pub struct OrgConnection {
    /// The organizations on this page.
    pub nodes: Vec<OrgNode>,
    /// The number of organizations in total.
    pub total_count: i32,
}

/// A page of users (an organization's members).
#[derive(SimpleObject)]
pub struct NewUsersConnection {
    /// The users on this page.
    pub nodes: Vec<UserNode>,
    /// The number of users in total.
    pub total_count: i32,
    /// Pagination.
    pub page_info: PageInfo,
}

/// A recipient's answer to an invitation.
#[derive(Enum, Copy, Clone, Debug, PartialEq, Eq)]
#[graphql(name = "OrganizationInvitationResponseType")]
pub enum InvitationResponse {
    /// Accepted: the recipient joined.
    Accept,
    /// Declined.
    Reject,
}

/// An invitation to join an organization.
#[derive(Debug, Clone)]
pub struct OrgInvitationNode(pub OrgInvitation);

#[Object(name = "OrganizationInvitation")]
impl OrgInvitationNode {
    /// The opaque node ID.
    pub async fn id(&self) -> ID {
        ID(marshal_id(ORG_INVITATION_KIND, self.0.id.get()))
    }

    /// The organization.
    async fn organization(&self, ctx: &Context<'_>) -> Result<OrgNode> {
        load_org(ctx, self.0.org_id).await.map(OrgNode)
    }

    /// Who sent the invitation; null once deleted.
    async fn sender(&self, ctx: &Context<'_>) -> Result<Option<UserNode>> {
        Ok(services(ctx)?
            .db
            .users()
            .get_by_id(self.0.sender_user_id)
            .await
            .map_err(db_error)?
            .map(UserNode))
    }

    /// The invited user (null for an invitation to an email address not yet answered).
    async fn recipient(&self, ctx: &Context<'_>) -> Result<Option<UserNode>> {
        let Some(id) = self.0.recipient_user_id else {
            return Ok(None);
        };
        Ok(services(ctx)?
            .db
            .users()
            .get_by_id(id)
            .await
            .map_err(db_error)?
            .map(UserNode))
    }

    /// The invited email address, for invitations by email.
    async fn recipient_email(&self) -> Option<&str> {
        self.0.recipient_email.as_deref()
    }

    /// When the invitation was created.
    async fn created_at(&self) -> DateTime<Utc> {
        self.0.created_at
    }

    /// When the recipient was last emailed about it.
    async fn notified_at(&self) -> Option<DateTime<Utc>> {
        self.0.notified_at
    }

    /// When the recipient answered.
    async fn responded_at(&self) -> Option<DateTime<Utc>> {
        self.0.responded_at
    }

    /// The answer, or null while pending.
    async fn response_type(&self) -> Option<InvitationResponse> {
        self.0.response_accepted.map(|a| {
            if a {
                InvitationResponse::Accept
            } else {
                InvitationResponse::Reject
            }
        })
    }

    /// Where the recipient answers it while pending: the organization's page (the link with
    /// the token is only in the invitation email and the `inviteUserToOrganization` result).
    #[graphql(name = "respondURL")]
    async fn respond_url(&self, ctx: &Context<'_>) -> Result<Option<String>> {
        if !self.0.is_pending() {
            return Ok(None);
        }
        let org = load_org(ctx, self.0.org_id).await?;
        Ok(Some(format!("/organizations/{}", org.name)))
    }

    /// When it was revoked.
    async fn revoked_at(&self) -> Option<DateTime<Utc>> {
        self.0.revoked_at
    }

    /// When it expires.
    async fn expires_at(&self) -> Option<DateTime<Utc>> {
        self.0.expires_at
    }

    /// For an invitation by email: whether the address is a verified address of the viewer.
    async fn is_verified_email(&self, ctx: &Context<'_>) -> Result<Option<bool>> {
        let (Some(email), Some(actor)) = (self.0.recipient_email.as_deref(), viewer(ctx)) else {
            return Ok(None);
        };
        let emails = verified_emails(ctx, actor.user_id).await?;
        Ok(Some(emails.iter().any(|e| e.eq_ignore_ascii_case(email))))
    }
}

/// The result of `inviteUserToOrganization`.
#[derive(SimpleObject)]
pub struct InviteUserToOrganizationResult {
    /// Whether an invitation email was sent (needs SMTP and a recipient address).
    pub sent_invitation_email: bool,
    /// The link the recipient opens to accept or decline.
    #[graphql(name = "invitationURL")]
    pub invitation_url: String,
}

async fn verified_emails(ctx: &Context<'_>, user: UserId) -> Result<Vec<String>> {
    Ok(services(ctx)?
        .db
        .user_emails()
        .list(user)
        .await
        .map_err(db_error)?
        .into_iter()
        .filter(|e| e.verified_at.is_some())
        .map(|e| e.email)
        .collect())
}

/// Pending invitations addressed to `user` or one of their verified addresses.
async fn viewer_invitations(ctx: &Context<'_>, user: UserId) -> Result<Vec<OrgInvitation>> {
    let emails = verified_emails(ctx, user).await?;
    services(ctx)?
        .db
        .orgs()
        .pending_invitations_for_user(user, &emails)
        .await
        .map_err(db_error)
}

/// Whether `user` is the recipient of `inv`.
async fn is_recipient(ctx: &Context<'_>, inv: &OrgInvitation, user: UserId) -> Result<bool> {
    if let Some(recipient) = inv.recipient_user_id {
        return Ok(recipient == user);
    }
    let Some(email) = inv.recipient_email.as_deref() else {
        return Ok(false);
    };
    Ok(verified_emails(ctx, user)
        .await?
        .iter()
        .any(|e| e.eq_ignore_ascii_case(email)))
}

fn token_hash(token: &str) -> [u8; 32] {
    sourcegull_crypto::session::hash_session_id(token)
}

/// The invitation link of `token`.
fn invitation_url(ctx: &Context<'_>, token: &str) -> Result<String> {
    let config = services(ctx)?.config.load();
    let origin = config
        .value
        .external_url
        .as_deref()
        .unwrap_or("")
        .trim_end_matches('/')
        .to_owned();
    Ok(format!("{origin}/organizations/invitation/{token}"))
}

/// The address an invitation is emailed to: the invited address, or the invited user's first
/// verified one.
async fn invitation_address(ctx: &Context<'_>, inv: &OrgInvitation) -> Result<Option<String>> {
    Ok(match (&inv.recipient_email, inv.recipient_user_id) {
        (Some(e), _) => Some(e.clone()),
        (None, Some(u)) => verified_emails(ctx, u).await?.into_iter().next(),
        (None, None) => None,
    })
}

/// Emails the invitation link when SMTP is configured; returns whether it was sent (and
/// records it on the invitation).
async fn send_invitation(
    ctx: &Context<'_>,
    inv: &OrgInvitation,
    sender: UserId,
    url: &str,
) -> Result<bool> {
    let svc = services(ctx)?;
    let Some(app) = &svc.app else {
        return Ok(false);
    };
    if svc.config.load().value.email_smtp.is_none() {
        return Ok(false);
    }
    let Some(to) = invitation_address(ctx, inv).await? else {
        return Ok(false);
    };
    let org = load_org(ctx, inv.org_id).await?;
    let sender = svc
        .db
        .users()
        .get_by_id(sender)
        .await
        .map_err(db_error)?
        .map(|u| u.username)
        .unwrap_or_default();
    let name = org.display_name.as_deref().unwrap_or(&org.name);
    app.send_email(sourcegull_services::email::link_message(
        &to,
        &format!("{sender} invited you to join {name}"),
        &format!("{sender} invited you to join the organization {name}. Accept or decline:"),
        url,
    ))
    .await;
    svc.db
        .orgs()
        .mark_notified(inv.id)
        .await
        .map_err(db_error)?;
    Ok(true)
}

/// Loads an invitation for the viewer: org members and site admins see all of the org's
/// invitations, recipients their own; others get `NOT_FOUND`.
async fn load_invitation(ctx: &Context<'_>, id: OrgInvitationId) -> Result<Option<OrgInvitation>> {
    let actor = require_viewer(ctx)?;
    let Some(inv) = services(ctx)?
        .db
        .orgs()
        .get_invitation(id)
        .await
        .map_err(db_error)?
    else {
        return Ok(None);
    };
    let visible = viewer_can_administer(ctx, inv.org_id).await?
        || is_recipient(ctx, &inv, actor.user_id).await?;
    Ok(visible.then_some(inv))
}

async fn require_invitation(ctx: &Context<'_>, id: OrgInvitationId) -> Result<OrgInvitation> {
    load_invitation(ctx, id)
        .await?
        .ok_or_else(|| error(codes::NOT_FOUND, "Invitation not found."))
}

/// Loads an invitation for `node(id:)`.
pub async fn load_invitation_node(ctx: &Context<'_>, id: i64) -> Result<Option<OrgInvitationNode>> {
    Ok(load_invitation(ctx, OrgInvitationId(id))
        .await?
        .map(OrgInvitationNode))
}

/// Organization queries.
#[derive(Default)]
pub struct OrgsQuery;

#[Object]
impl OrgsQuery {
    /// Lists organizations whose name or display name matches `query` (site admins only).
    async fn organizations(
        &self,
        ctx: &Context<'_>,
        first: Option<i32>,
        query: Option<String>,
    ) -> Result<OrgConnection> {
        require_site_admin(ctx)?;
        let orgs = services(ctx)?.db.orgs();
        let query = query.as_deref().map(str::trim);
        let nodes = orgs
            .list(query, page_size(first, 50, 1000), 0)
            .await
            .map_err(db_error)?;
        let total = orgs.count(query).await.map_err(db_error)?;
        Ok(OrgConnection {
            nodes: nodes.into_iter().map(OrgNode).collect(),
            total_count: to_int(total),
        })
    }

    /// Looks up an organization by name (signed-in users only).
    async fn organization(&self, ctx: &Context<'_>, name: String) -> Result<Option<OrgNode>> {
        require_viewer(ctx)?;
        let org = services(ctx)?
            .db
            .orgs()
            .get_by_name(&name)
            .await
            .map_err(db_error)?;
        Ok(org.map(OrgNode))
    }

    /// The pending invitations of an organization, newest first (members and site admins).
    async fn pending_invitations(
        &self,
        ctx: &Context<'_>,
        org: ID,
    ) -> Result<Vec<OrgInvitationNode>> {
        let org = parse_org_id(&org)?;
        load_org(ctx, org).await?;
        require_member_or_admin(ctx, org).await?;
        Ok(services(ctx)?
            .db
            .orgs()
            .pending_invitations(org)
            .await
            .map_err(db_error)?
            .into_iter()
            .map(OrgInvitationNode)
            .collect())
    }

    /// The viewer's pending invitations, newest first (for the user's
    /// invitations list).
    async fn viewer_organization_invitations(
        &self,
        ctx: &Context<'_>,
    ) -> Result<Vec<OrgInvitationNode>> {
        let actor = require_viewer(ctx)?;
        Ok(viewer_invitations(ctx, actor.user_id)
            .await?
            .into_iter()
            .map(OrgInvitationNode)
            .collect())
    }

    /// The invitation of a link token. An invitation of a user is shown to that user only.
    async fn invitation_by_token(
        &self,
        ctx: &Context<'_>,
        token: String,
    ) -> Result<OrgInvitationNode> {
        let actor = require_viewer(ctx)?;
        let inv = services(ctx)?
            .db
            .orgs()
            .get_invitation_by_token(&token_hash(&token))
            .await
            .map_err(db_error)?
            .filter(|i| i.recipient_user_id.is_none_or(|u| u == actor.user_id))
            .ok_or_else(|| error(codes::NOT_FOUND, "Invitation not found."))?;
        Ok(OrgInvitationNode(inv))
    }
}

/// Organization mutations.
#[derive(Default)]
pub struct OrgsMutation;

#[Object]
impl OrgsMutation {
    /// Creates an organization; the creator becomes its first member.
    async fn create_organization(
        &self,
        ctx: &Context<'_>,
        name: String,
        display_name: Option<String>,
    ) -> Result<OrgNode> {
        let actor = require_viewer(ctx)?;
        let svc = services(ctx)?;
        let name = name.trim();
        // Org names share the username rules (they live in the same namespace in URLs).
        sourcegull_authn::validate::validate_username(name).map_err(auth_error)?;
        let display_name = display_name
            .as_deref()
            .map(str::trim)
            .filter(|d| !d.is_empty());
        let taken = || {
            error(
                codes::ALREADY_EXISTS,
                format!("The name {name:?} is already taken by a user, an organization or a team."),
            )
        };
        let user = svc
            .db
            .users()
            .get_by_username(name)
            .await
            .map_err(db_error)?;
        let team = sourcegull_own_graphql::teams::own_store(ctx)?
            .get_team_by_name(name)
            .await
            .map_err(db_error)?;
        if user.is_some() || team.is_some() {
            return Err(taken());
        }
        let orgs = svc.db.orgs();
        let org = orgs.create(name, display_name).await.map_err(|e| {
            if matches!(e, sourcegull_db::DbError::AlreadyExists { .. }) {
                taken()
            } else {
                db_error(e)
            }
        })?;
        orgs.add_member(org.id, actor.user_id, OrgRole::Admin)
            .await
            .map_err(db_error)?;
        audit::record(
            svc,
            events::ORG_CREATED,
            Some(actor.user_id),
            json!({ "orgID": org.id, "name": org.name }),
        )
        .await;
        Ok(OrgNode(org))
    }

    /// Changes the display name (`""` clears it). Members and site admins.
    async fn update_organization(
        &self,
        ctx: &Context<'_>,
        id: ID,
        display_name: Option<String>,
    ) -> Result<OrgNode> {
        let org = parse_org_id(&id)?;
        load_org(ctx, org).await?;
        require_member_or_admin(ctx, org).await?;
        let display_name = display_name
            .as_deref()
            .map(str::trim)
            .filter(|d| !d.is_empty());
        let org = services(ctx)?
            .db
            .orgs()
            .update(org, display_name)
            .await
            .map_err(db_error)?;
        Ok(OrgNode(org))
    }

    /// Deletes an organization (soft delete; its name becomes free and pending invitations
    /// are revoked). Site admins only.
    async fn delete_organization(
        &self,
        ctx: &Context<'_>,
        organization: ID,
    ) -> Result<Option<EmptyResponse>> {
        let actor = require_site_admin(ctx)?;
        let org = parse_org_id(&organization)?;
        let svc = services(ctx)?;
        svc.db.orgs().soft_delete(org).await.map_err(db_error)?;
        audit::record(
            svc,
            events::ORG_DELETED,
            Some(actor.user_id),
            json!({ "orgID": org }),
        )
        .await;
        Ok(Some(EmptyResponse))
    }

    /// Adds a user as a member right away, without an invitation. Site admins only.
    async fn add_user_to_organization(
        &self,
        ctx: &Context<'_>,
        organization: ID,
        username: String,
    ) -> Result<EmptyResponse> {
        let actor = require_site_admin(ctx)?;
        let org = parse_org_id(&organization)?;
        load_org(ctx, org).await?;
        let svc = services(ctx)?;
        let user = svc
            .db
            .users()
            .get_by_username(username.trim())
            .await
            .map_err(db_error)?
            .ok_or_else(|| error(codes::NOT_FOUND, format!("No user named {username:?}.")))?;
        svc.db
            .orgs()
            .add_member(org, user.id, OrgRole::Member)
            .await
            .map_err(|e| match e {
                sourcegull_db::DbError::AlreadyExists { .. } => error(
                    codes::ALREADY_EXISTS,
                    format!("{} is already a member.", user.username),
                ),
                other => db_error(other),
            })?;
        audit::record(
            svc,
            events::ORG_MEMBER_ADDED,
            Some(actor.user_id),
            json!({ "orgID": org, "userID": user.id }),
        )
        .await;
        Ok(EmptyResponse)
    }

    /// Removes a member. Members (including the member themself) and site admins.
    async fn remove_user_from_organization(
        &self,
        ctx: &Context<'_>,
        user: ID,
        organization: ID,
    ) -> Result<Option<EmptyResponse>> {
        let org = parse_org_id(&organization)?;
        let user = parse_user_id(&user)?;
        load_org(ctx, org).await?;
        require_member_or_admin(ctx, org).await?;
        let svc = services(ctx)?;
        svc.db
            .orgs()
            .remove_member(org, user)
            .await
            .map_err(db_error)?;
        audit::record(
            svc,
            events::ORG_MEMBER_REMOVED,
            viewer(ctx).map(|a| a.user_id),
            json!({ "orgID": org, "userID": user }),
        )
        .await;
        Ok(Some(EmptyResponse))
    }

    /// Invites a user by username, or anyone by email address (Sourcegull: `email`; an address
    /// verified by a user invites that user). Members and site admins. The result carries
    /// the invitation link; it is also emailed when SMTP is configured.
    async fn invite_user_to_organization(
        &self,
        ctx: &Context<'_>,
        organization: ID,
        username: Option<String>,
        email: Option<String>,
    ) -> Result<InviteUserToOrganizationResult> {
        let actor = require_viewer(ctx)?;
        let org_id = parse_org_id(&organization)?;
        load_org(ctx, org_id).await?;
        require_member_or_admin(ctx, org_id).await?;
        let db = &services(ctx)?.db;
        let username = username.as_deref().map(str::trim).filter(|s| !s.is_empty());
        let email = email.as_deref().map(str::trim).filter(|s| !s.is_empty());
        let (recipient, recipient_email) = match (username, email) {
            (Some(name), None) => {
                let user = db
                    .users()
                    .get_by_username(name)
                    .await
                    .map_err(db_error)?
                    .ok_or_else(|| error(codes::NOT_FOUND, format!("No user named {name:?}.")))?;
                (Some(user.id), None)
            }
            (None, Some(email)) => {
                if !email.contains('@') {
                    return Err(error(codes::INVALID_INPUT, "Invalid email address."));
                }
                let user = db
                    .user_emails()
                    .user_by_verified_email(email)
                    .await
                    .map_err(db_error)?;
                (user, user.is_none().then(|| email.to_owned()))
            }
            _ => {
                return Err(error(
                    codes::INVALID_INPUT,
                    "Invite either a username or an email address.",
                ));
            }
        };
        if let Some(u) = recipient
            && db.orgs().is_member(org_id, u).await.map_err(db_error)?
        {
            return Err(error(
                codes::ALREADY_EXISTS,
                "The user is already a member of the organization.",
            ));
        }
        let token = sourcegull_crypto::session::generate_session_id();
        let inv = db
            .orgs()
            .create_invitation(&NewOrgInvitation {
                org_id,
                sender: actor.user_id,
                recipient_user: recipient,
                recipient_email: recipient_email.as_deref(),
                token_sha256: &token_hash(&token),
                expires_at: Some(Utc::now() + INVITATION_EXPIRY),
            })
            .await
            .map_err(db_error)?;
        let url = invitation_url(ctx, &token)?;
        let sent = send_invitation(ctx, &inv, actor.user_id, &url).await?;
        Ok(InviteUserToOrganizationResult {
            sent_invitation_email: sent,
            invitation_url: url,
        })
    }

    /// Accepts or declines an invitation. Its recipient only.
    async fn respond_to_organization_invitation(
        &self,
        ctx: &Context<'_>,
        organization_invitation: ID,
        response_type: InvitationResponse,
    ) -> Result<EmptyResponse> {
        let actor = require_viewer(ctx)?;
        let id = parse_invitation_id(&organization_invitation)?;
        let db = &services(ctx)?.db;
        let inv = db
            .orgs()
            .get_invitation(id)
            .await
            .map_err(db_error)?
            .ok_or_else(|| error(codes::NOT_FOUND, "Invitation not found."))?;
        if !is_recipient(ctx, &inv, actor.user_id).await? {
            return Err(error(
                codes::NOT_FOUND,
                "Invitation not found (it is for another user or email address).",
            ));
        }
        db.orgs()
            .respond_to_invitation(
                id,
                actor.user_id,
                response_type == InvitationResponse::Accept,
            )
            .await
            .map_err(|e| match e {
                sourcegull_db::DbError::NotFound(_) => error(
                    codes::CONFLICT,
                    "The invitation was already answered, revoked or has expired.",
                ),
                other => db_error(other),
            })?;
        Ok(EmptyResponse)
    }

    /// Revokes a pending invitation. Members and site admins.
    async fn revoke_organization_invitation(
        &self,
        ctx: &Context<'_>,
        organization_invitation: ID,
    ) -> Result<EmptyResponse> {
        let id = parse_invitation_id(&organization_invitation)?;
        let inv = require_invitation(ctx, id).await?;
        require_member_or_admin(ctx, inv.org_id).await?;
        services(ctx)?
            .db
            .orgs()
            .revoke_invitation(id)
            .await
            .map_err(|e| match e {
                sourcegull_db::DbError::NotFound(_) => {
                    error(codes::CONFLICT, "The invitation is no longer pending.")
                }
                other => db_error(other),
            })?;
        Ok(EmptyResponse)
    }

    /// Emails a pending invitation again with a new link (the old link stops working).
    /// Members and site admins.
    async fn resend_organization_invitation_notification(
        &self,
        ctx: &Context<'_>,
        organization_invitation: ID,
    ) -> Result<EmptyResponse> {
        let actor = require_viewer(ctx)?;
        let id = parse_invitation_id(&organization_invitation)?;
        let inv = require_invitation(ctx, id).await?;
        require_member_or_admin(ctx, inv.org_id).await?;
        if !inv.is_pending() {
            return Err(error(
                codes::CONFLICT,
                "The invitation is no longer pending.",
            ));
        }
        if invitation_address(ctx, &inv).await?.is_none() {
            return Err(error(
                codes::INVALID_INPUT,
                "The recipient has no verified email address.",
            ));
        }
        let token = sourcegull_crypto::session::generate_session_id();
        services(ctx)?
            .db
            .orgs()
            .set_invitation_token(id, &token_hash(&token))
            .await
            .map_err(db_error)?;
        let url = invitation_url(ctx, &token)?;
        if !send_invitation(ctx, &inv, actor.user_id, &url).await? {
            return Err(error(
                codes::UNAVAILABLE,
                "Email is not configured (email.smtp in the site configuration).",
            ));
        }
        Ok(EmptyResponse)
    }
}
