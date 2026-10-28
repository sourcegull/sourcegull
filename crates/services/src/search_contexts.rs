//! Who may see and change search contexts.
//!
//! Who may read a context is the store's rule ([`ContextAccess`]). Writing needs: site admin
//! for instance-level contexts and for any public one, the user for their own, membership for
//! an org's. Stars and defaults are the user's own (or a site admin's to change).

use sourcegull_authn::{AccessError, Actor};
use sourcegull_db::{ContextAccess, Db, DbError, Namespace};
use sourcegull_types::{OrgId, UserId};

/// The store access of `actor` (anonymous: `None`).
pub fn access(actor: Option<&Actor>) -> ContextAccess {
    ContextAccess::Viewer(actor.map(|a| a.user_id))
}

/// What deciding whether `actor` may write a context takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Check {
    Allow,
    Deny,
    Member(OrgId),
}

fn write_check(actor: &Actor, ns: Option<Namespace>, public: bool) -> Check {
    if actor.site_admin && public {
        return Check::Allow;
    }
    match ns {
        None if actor.site_admin => Check::Allow,
        None => Check::Deny,
        Some(Namespace::User(u)) if u == actor.user_id => Check::Allow,
        Some(Namespace::User(_)) => Check::Deny,
        Some(Namespace::Org(o)) => Check::Member(o),
    }
}

/// Whether `actor` may write a context owned by `ns` (`None`: instance-level) with
/// visibility `public`.
pub async fn can_write(
    db: &Db,
    actor: &Actor,
    ns: Option<Namespace>,
    public: bool,
) -> Result<bool, DbError> {
    Ok(match write_check(actor, ns, public) {
        Check::Allow => true,
        Check::Deny => false,
        Check::Member(o) => db.orgs().is_member(o, actor.user_id).await?,
    })
}

/// Fails with [`AccessError::Forbidden`] unless [`can_write`].
pub async fn require_write(
    db: &Db,
    actor: &Actor,
    ns: Option<Namespace>,
    public: bool,
) -> Result<(), AccessError> {
    if can_write(db, actor, ns, public).await? {
        Ok(())
    } else {
        Err(AccessError::forbidden(
            "You may not manage search contexts here.",
        ))
    }
}

/// Fails with [`AccessError::Forbidden`] unless `actor` is `user` or a site admin (stars and
/// defaults).
pub fn require_same_user(actor: &Actor, user: UserId) -> Result<(), AccessError> {
    if actor.is_self_or_site_admin(user) {
        Ok(())
    } else {
        Err(AccessError::forbidden(
            "You may only change your own stars and default.",
        ))
    }
}

/// The error for a context that is missing or that the actor may not read.
pub fn not_found() -> AccessError {
    AccessError::NotFound("Search context")
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
    fn write_rule() {
        let alice = actor(1, false);
        let admin = actor(9, true);
        let alice_ns = Some(Namespace::User(UserId(1)));
        let org = Some(Namespace::Org(OrgId(5)));
        assert_eq!(write_check(&alice, alice_ns, false), Check::Allow);
        assert_eq!(write_check(&alice, alice_ns, true), Check::Allow);
        assert_eq!(write_check(&alice, None, false), Check::Deny);
        assert_eq!(
            write_check(&alice, Some(Namespace::User(UserId(2))), false),
            Check::Deny
        );
        assert_eq!(write_check(&alice, org, false), Check::Member(OrgId(5)));
        assert_eq!(write_check(&admin, None, false), Check::Allow);
        // Site admins write any public context, but another user's private one only as
        // that user.
        assert_eq!(write_check(&admin, alice_ns, true), Check::Allow);
        assert_eq!(write_check(&admin, alice_ns, false), Check::Deny);
        assert_eq!(write_check(&admin, org, false), Check::Member(OrgId(5)));
    }

    #[test]
    fn stars_and_defaults_are_the_users_own() {
        assert!(require_same_user(&actor(1, false), UserId(1)).is_ok());
        assert!(require_same_user(&actor(9, true), UserId(1)).is_ok());
        let e = require_same_user(&actor(2, false), UserId(1)).unwrap_err();
        assert_eq!(e.code(), "FORBIDDEN");
    }

    #[test]
    fn hidden_contexts_are_not_found() {
        assert_eq!(not_found().to_string(), "Search context not found.");
        assert!(matches!(access(None), ContextAccess::Viewer(None)));
    }
}
