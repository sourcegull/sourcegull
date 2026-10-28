//! Who may see and manage watches.
//!
//! A watch is visible to, and managed by, whoever may act for its owner
//! ([`can_act_for`](sourcegull_authn::access::can_act_for): the user, the org's members, site
//! admins); to everyone else it does not exist. Email recipients must be namespaces the actor
//! may act for. A user's watch list is readable by the user and site admins.
//!
//! Email recipients get only the results in repositories they can read, unless the watch is
//! unfiltered ([`unfiltered_owner`]): then everyone gets every result the watch finds with
//! the access of the user it runs as. Only site admins may make a watch unfiltered, and other
//! users too when `watches.allowUnfilteredResults` is on.

use sourcegull_authn::access::{can_act_for, require_can_act_for, require_signed_in};
use sourcegull_authn::{AccessError, Actor};
use sourcegull_db::{Db, DbError, Namespace};
use sourcegull_types::UserId;

use sourcegull_config::SiteConfig;

use crate::Monitor;
use crate::trigger::owner_actor;

/// Whether `actor` (anonymous: `None`) may see `m`.
pub async fn can_view(db: &Db, actor: Option<&Actor>, m: &Monitor) -> Result<bool, DbError> {
    match actor {
        Some(actor) => can_act_for(db, actor, m.namespace).await,
        None => Ok(false),
    }
}

/// Fails with [`AccessError::NotFound`] (as for a missing watch) unless `actor` may manage
/// `m`.
pub async fn require_can_manage(db: &Db, actor: &Actor, m: &Monitor) -> Result<(), AccessError> {
    if can_act_for(db, actor, m.namespace).await? {
        Ok(())
    } else {
        Err(not_found())
    }
}

/// The error for a watch that is missing or that the actor may not see.
pub fn not_found() -> AccessError {
    AccessError::NotFound("Watch")
}

/// Fails with [`AccessError::Forbidden`] unless `actor` may own watches in `ns` (create them
/// there, move them there, send test actions for them).
pub async fn require_can_own(db: &Db, actor: &Actor, ns: Namespace) -> Result<(), AccessError> {
    require_can_act_for(db, actor, ns).await
}

/// Fails with [`AccessError::Forbidden`] unless `actor` may send email to `recipient`.
pub async fn require_can_email(
    db: &Db,
    actor: &Actor,
    recipient: Namespace,
) -> Result<(), AccessError> {
    if can_act_for(db, actor, recipient).await? {
        Ok(())
    } else {
        Err(AccessError::forbidden(
            "Email recipients must be yourself or organizations you belong to.",
        ))
    }
}

/// Whether `actor` may make a watch deliver unfiltered results.
pub fn may_share_unfiltered(actor: &Actor, config: &SiteConfig) -> bool {
    actor.site_admin || config.watches_allow_unfiltered_results
}

/// Fails with [`AccessError::Forbidden`] unless `actor` may make a watch unfiltered.
pub fn require_may_share_unfiltered(actor: &Actor, config: &SiteConfig) -> Result<(), AccessError> {
    if may_share_unfiltered(actor, config) {
        Ok(())
    } else {
        Err(AccessError::forbidden(
            "Only site admins may make a watch send unfiltered results.",
        ))
    }
}

/// The user whose access unfiltered watch `m` shares with every email recipient, or `None`
/// when each recipient gets only the results they can read: `m` must have been made
/// unfiltered by the user it runs as, who must still be allowed to.
pub async fn unfiltered_owner(
    db: &Db,
    config: &SiteConfig,
    m: &Monitor,
) -> Result<Option<Actor>, DbError> {
    let Some(by) = m.unfiltered_by else {
        return Ok(None);
    };
    Ok(owner_actor(db, m)
        .await?
        .filter(|a| a.user_id == by && may_share_unfiltered(a, config)))
}

/// The actor, if they may list the watches of `user` (theirs and their orgs'): the user
/// themself or a site admin.
pub fn require_can_list_user(actor: Option<&Actor>, user: UserId) -> Result<&Actor, AccessError> {
    let actor = require_signed_in(actor)?;
    if actor.is_self_or_site_admin(user) {
        Ok(actor)
    } else {
        Err(AccessError::forbidden(
            "Only the user and site admins may list the user's watches.",
        ))
    }
}

#[cfg(test)]
mod tests {
    use sourcegull_authn::{AuthVia, PermissionSet};

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
    fn user_watch_list() {
        let alice = actor(1, false);
        let admin = actor(9, true);
        assert!(matches!(
            require_can_list_user(None, UserId(1)),
            Err(AccessError::SignInRequired)
        ));
        assert!(require_can_list_user(Some(&alice), UserId(1)).is_ok());
        assert!(require_can_list_user(Some(&admin), UserId(1)).is_ok());
        let e = require_can_list_user(Some(&alice), UserId(2)).unwrap_err();
        assert_eq!(e.code(), "FORBIDDEN");
    }

    #[test]
    fn unfiltered_results_need_a_site_admin_or_the_setting() {
        let mut config = SiteConfig::default();
        assert!(may_share_unfiltered(&actor(9, true), &config));
        assert!(!may_share_unfiltered(&actor(1, false), &config));
        let e = require_may_share_unfiltered(&actor(1, false), &config).unwrap_err();
        assert_eq!(e.code(), "FORBIDDEN");
        config.watches_allow_unfiltered_results = true;
        assert!(may_share_unfiltered(&actor(1, false), &config));
    }

    #[test]
    fn hidden_watches_are_not_found() {
        assert_eq!(not_found().code(), "NOT_FOUND");
        assert_eq!(not_found().to_string(), "Watch not found.");
    }
}
