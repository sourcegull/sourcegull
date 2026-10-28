//! Who may see and manage notebooks.
//!
//! Public notebooks are readable by everyone who may use the instance, private ones only by
//! the owning user or the owning org's members (the SQL of [`Access::Viewer`]). Managing
//! (editing, deleting) needs the same affiliation with the namespace; site admins get no
//! bypass.

use sourcegull_authn::{AccessError, Actor};
use sourcegull_db::{Db, DbError, Namespace};
use sourcegull_types::OrgId;

use crate::Access;

/// The store access of `actor` (anonymous: `None`).
pub fn access(actor: Option<&Actor>) -> Access {
    Access::Viewer(actor.map(|a| a.user_id))
}

/// What deciding whether `actor` may manage notebooks of a namespace takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Check {
    Allow,
    Deny,
    Member(OrgId),
}

fn manage_check(actor: &Actor, ns: Namespace) -> Check {
    match ns {
        Namespace::User(u) if actor.user_id == u => Check::Allow,
        Namespace::User(_) => Check::Deny,
        Namespace::Org(o) => Check::Member(o),
    }
}

/// Whether `actor` may create, edit and delete notebooks of `ns`: the user themself or a
/// member of the org.
pub async fn can_manage(db: &Db, actor: &Actor, ns: Namespace) -> Result<bool, DbError> {
    Ok(match manage_check(actor, ns) {
        Check::Allow => true,
        Check::Deny => false,
        Check::Member(o) => db.orgs().is_member(o, actor.user_id).await?,
    })
}

/// [`can_manage`] for a possibly anonymous viewer (anonymous: `false`).
pub async fn viewer_can_manage(
    db: &Db,
    actor: Option<&Actor>,
    ns: Namespace,
) -> Result<bool, DbError> {
    match actor {
        Some(actor) => can_manage(db, actor, ns).await,
        None => Ok(false),
    }
}

/// Fails with [`AccessError::Forbidden`] unless `actor` may manage notebooks of `ns`.
pub async fn require_can_manage(db: &Db, actor: &Actor, ns: Namespace) -> Result<(), AccessError> {
    if can_manage(db, actor, ns).await? {
        Ok(())
    } else {
        Err(manage_denied(ns))
    }
}

fn manage_denied(ns: Namespace) -> AccessError {
    AccessError::forbidden(match ns {
        Namespace::User(_) => "User does not match the notebook user namespace.",
        Namespace::Org(_) => "User is not a member of the notebook organization namespace.",
    })
}

#[cfg(test)]
mod tests {
    use sourcegull_authn::{AuthVia, PermissionSet};
    use sourcegull_types::UserId;

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
    fn manage_rule() {
        let alice = actor(1, false);
        let admin = actor(9, true);
        assert_eq!(
            manage_check(&alice, Namespace::User(UserId(1))),
            Check::Allow
        );
        assert_eq!(
            manage_check(&alice, Namespace::User(UserId(2))),
            Check::Deny
        );
        // Site admins get no bypass.
        assert_eq!(
            manage_check(&admin, Namespace::User(UserId(1))),
            Check::Deny
        );
        assert_eq!(
            manage_check(&admin, Namespace::Org(OrgId(3))),
            Check::Member(OrgId(3))
        );
    }

    #[test]
    fn denial_messages() {
        let user = manage_denied(Namespace::User(UserId(1)));
        assert_eq!(user.code(), "FORBIDDEN");
        assert_eq!(
            user.to_string(),
            "User does not match the notebook user namespace."
        );
        assert_eq!(
            manage_denied(Namespace::Org(OrgId(1))).to_string(),
            "User is not a member of the notebook organization namespace."
        );
    }

    #[test]
    fn anonymous_access() {
        assert!(matches!(access(None), Access::Viewer(None)));
        assert!(matches!(
            access(Some(&actor(4, false))),
            Access::Viewer(Some(UserId(4)))
        ));
    }
}
