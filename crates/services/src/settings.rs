//! Who may read and write a subject's settings.
//!
//! Global settings are readable by everyone and written by site admins; an organization's by
//! its members and site admins; a user's by the user and site admins.

use sourcegull_authn::{AccessError, Actor};
use sourcegull_db::{Db, DbError};
use sourcegull_types::{OrgId, SettingsSubject};

/// What deciding whether an actor may write a subject's settings takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Check {
    Allow,
    Deny,
    Member(OrgId),
}

fn write_check(actor: &Actor, subject: SettingsSubject) -> Check {
    if actor.site_admin {
        return Check::Allow;
    }
    match subject {
        SettingsSubject::Global => Check::Deny,
        SettingsSubject::User(u) if u == actor.user_id => Check::Allow,
        SettingsSubject::User(_) => Check::Deny,
        SettingsSubject::Org(o) => Check::Member(o),
    }
}

/// Whether `actor` (anonymous: `None`) may read `subject`'s settings.
pub async fn can_read(
    db: &Db,
    actor: Option<&Actor>,
    subject: SettingsSubject,
) -> Result<bool, DbError> {
    match (subject, actor) {
        (SettingsSubject::Global, _) => Ok(true),
        (_, Some(a)) => can_write(db, a, subject).await,
        (_, None) => Ok(false),
    }
}

/// Whether `actor` may change `subject`'s settings.
pub async fn can_write(db: &Db, actor: &Actor, subject: SettingsSubject) -> Result<bool, DbError> {
    Ok(match write_check(actor, subject) {
        Check::Allow => true,
        Check::Deny => false,
        Check::Member(o) => db.orgs().is_member(o, actor.user_id).await?,
    })
}

/// Fails with [`AccessError::Forbidden`] unless `actor` may read `subject`'s settings.
pub async fn require_read(
    db: &Db,
    actor: Option<&Actor>,
    subject: SettingsSubject,
) -> Result<(), AccessError> {
    if can_read(db, actor, subject).await? {
        Ok(())
    } else {
        Err(AccessError::forbidden(
            "Only members of the organization (or the user) and site admins may see these \
             settings.",
        ))
    }
}

/// Fails with [`AccessError::Forbidden`] unless `actor` may change `subject`'s settings.
pub async fn require_write(
    db: &Db,
    actor: &Actor,
    subject: SettingsSubject,
) -> Result<(), AccessError> {
    if can_write(db, actor, subject).await? {
        Ok(())
    } else {
        Err(AccessError::forbidden(
            "Only members of the organization (or the user) and site admins may change these \
             settings.",
        ))
    }
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
    fn write_rule() {
        let alice = actor(1, false);
        let admin = actor(9, true);
        assert_eq!(write_check(&alice, SettingsSubject::Global), Check::Deny);
        assert_eq!(write_check(&admin, SettingsSubject::Global), Check::Allow);
        assert_eq!(
            write_check(&alice, SettingsSubject::User(UserId(1))),
            Check::Allow
        );
        assert_eq!(
            write_check(&alice, SettingsSubject::User(UserId(2))),
            Check::Deny
        );
        assert_eq!(
            write_check(&admin, SettingsSubject::User(UserId(2))),
            Check::Allow
        );
        assert_eq!(
            write_check(&alice, SettingsSubject::Org(OrgId(3))),
            Check::Member(OrgId(3))
        );
        assert_eq!(
            write_check(&admin, SettingsSubject::Org(OrgId(3))),
            Check::Allow
        );
    }
}
