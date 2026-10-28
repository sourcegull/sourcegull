//! Who may see and change trends, dashboards and the backfill queue. The GraphQL API and the
//! export route ([`crate::http`]) decide through these functions.
//!
//! Views and dashboards the actor does not see are [`AccessError::NotFound`], so their
//! existence does not leak.

use sourcegull_authn::Actor;
use sourcegull_authn::access::{AccessError, require_signed_in, require_site_admin};

use crate::views::{Grant, Principal, View, ViewStore};

/// The message for a dashboard grant the actor may not give.
pub const GRANT_FORBIDDEN: &str =
    "You may only grant a dashboard to yourself, your organizations or everyone.";

/// The signed-in actor as a grant principal (their user and organizations).
pub async fn principal(views: &ViewStore, actor: Option<&Actor>) -> Result<Principal, AccessError> {
    let actor = require_signed_in(actor)?;
    Ok(views.principal(Some(actor.user_id)).await?)
}

/// The view with `unique_id` if `who` sees it.
pub async fn visible_view(
    views: &ViewStore,
    who: &Principal,
    unique_id: &str,
) -> Result<View, AccessError> {
    let view = views
        .view_by_unique_id(unique_id)
        .await?
        .ok_or(AccessError::NotFound("Trend"))?;
    // 🚨 SECURITY: views the viewer does not see are "not found", so they do not leak.
    if !views.view_visible(who, view.id).await? {
        return Err(AccessError::NotFound("Trend"));
    }
    Ok(view)
}

/// Fails with [`AccessError::NotFound`] unless `who` sees the dashboard `id`.
pub async fn require_dashboard_visible(
    views: &ViewStore,
    who: &Principal,
    id: i64,
) -> Result<(), AccessError> {
    if views.dashboard_visible(who, id).await? {
        Ok(())
    } else {
        Err(AccessError::NotFound("Dashboard"))
    }
}

/// Fails with [`AccessError::Forbidden`] unless `who` may give every one of `grants`.
pub fn require_may_grant(who: &Principal, grants: &[Grant]) -> Result<(), AccessError> {
    if who.may_grant(grants) {
        Ok(())
    } else {
        Err(AccessError::forbidden(GRANT_FORBIDDEN))
    }
}

/// The actor if they may see and change the backfill queue, which shows every trend's title
/// and query: site admins only.
pub fn require_queue_admin(actor: Option<&Actor>) -> Result<&Actor, AccessError> {
    require_site_admin(actor)
}

#[cfg(test)]
mod tests {
    use sourcegull_authn::{AuthVia, PermissionSet};
    use sourcegull_types::{OrgId, UserId};

    use super::*;

    fn actor(id: i64, site_admin: bool) -> Actor {
        Actor {
            user_id: UserId(id),
            username: format!("u{id}"),
            site_admin,
            via: AuthVia::Session,
            permissions: PermissionSet::default(),
        }
    }

    #[test]
    fn grants_go_to_oneself_ones_orgs_or_everyone() {
        let who = Principal {
            user: Some(UserId(1)),
            orgs: vec![7],
        };
        assert!(require_may_grant(&who, &[Grant::User(UserId(1)), Grant::Org(OrgId(7))]).is_ok());
        assert!(require_may_grant(&who, &[Grant::Global]).is_ok());
        assert!(require_may_grant(&who, &[]).is_ok());
        let e = require_may_grant(&who, &[Grant::User(UserId(2))]).unwrap_err();
        assert_eq!(e.code(), "FORBIDDEN");
        assert_eq!(e.to_string(), GRANT_FORBIDDEN);
        assert!(require_may_grant(&who, &[Grant::Org(OrgId(8))]).is_err());
    }

    #[test]
    fn queue_is_for_site_admins() {
        assert!(matches!(
            require_queue_admin(None),
            Err(AccessError::SignInRequired)
        ));
        let user = actor(1, false);
        assert_eq!(
            require_queue_admin(Some(&user)).unwrap_err().code(),
            "FORBIDDEN"
        );
        assert!(require_queue_admin(Some(&actor(2, true))).is_ok());
    }
}
