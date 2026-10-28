//! Who may create and change teams: any signed-in user creates a team; members and site
//! admins manage it; read-only teams only site admins. The GraphQL API decides through
//! these functions.

use sourcegull_authn::Actor;
use sourcegull_authn::access::{AccessError, require_signed_in};
use sourcegull_db::DbError;

use crate::store::{OwnStore, Team};

/// Whether `actor` may manage `team`: site admins always, members unless it is read-only.
pub async fn can_administer(store: &OwnStore, actor: &Actor, team: &Team) -> Result<bool, DbError> {
    match decided_without_membership(actor, team) {
        Some(may) => Ok(may),
        None => store.is_member(team.id, actor.user_id).await,
    }
}

/// Fails unless the signed-in `actor` may manage `team` ([`can_administer`]).
pub async fn require_administer(
    store: &OwnStore,
    actor: Option<&Actor>,
    team: &Team,
) -> Result<(), AccessError> {
    let actor = require_signed_in(actor)?;
    if can_administer(store, actor, team).await? {
        Ok(())
    } else {
        Err(AccessError::forbidden(denied_message(team)))
    }
}

/// The signed-in actor if they may create a team; read-only teams need a site admin.
pub fn require_may_create(actor: Option<&Actor>, readonly: bool) -> Result<&Actor, AccessError> {
    let actor = require_signed_in(actor)?;
    if readonly && !actor.site_admin {
        return Err(AccessError::forbidden(
            "Only site admins may create read-only teams.",
        ));
    }
    Ok(actor)
}

/// The decision when it does not depend on membership: site admins may, nobody else may
/// change a read-only team.
fn decided_without_membership(actor: &Actor, team: &Team) -> Option<bool> {
    if actor.site_admin {
        Some(true)
    } else if team.readonly {
        Some(false)
    } else {
        None
    }
}

fn denied_message(team: &Team) -> &'static str {
    if team.readonly {
        "The team is read-only; only site admins may change it."
    } else {
        "Only team members and site admins may change the team."
    }
}

#[cfg(test)]
mod tests {
    use sourcegull_authn::{AuthVia, PermissionSet};
    use sourcegull_types::UserId;

    use super::*;

    fn actor(site_admin: bool) -> Actor {
        Actor {
            user_id: UserId(1),
            username: "u".into(),
            site_admin,
            via: AuthVia::Session,
            permissions: PermissionSet::default(),
        }
    }

    fn team(readonly: bool) -> Team {
        Team {
            id: 1,
            name: "t".into(),
            display_name: None,
            readonly,
            parent_team_id: None,
            creator_id: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            external_service_id: None,
        }
    }

    #[test]
    fn site_admins_always_members_unless_read_only() {
        assert_eq!(
            decided_without_membership(&actor(true), &team(true)),
            Some(true)
        );
        assert_eq!(
            decided_without_membership(&actor(true), &team(false)),
            Some(true)
        );
        assert_eq!(
            decided_without_membership(&actor(false), &team(true)),
            Some(false)
        );
        assert_eq!(
            decided_without_membership(&actor(false), &team(false)),
            None
        );
        assert!(denied_message(&team(true)).contains("read-only"));
        assert!(denied_message(&team(false)).starts_with("Only team members"));
    }

    #[test]
    fn read_only_teams_are_created_by_site_admins() {
        assert!(matches!(
            require_may_create(None, false),
            Err(AccessError::SignInRequired)
        ));
        assert!(require_may_create(Some(&actor(false)), false).is_ok());
        assert_eq!(
            require_may_create(Some(&actor(false)), true)
                .unwrap_err()
                .code(),
            "FORBIDDEN"
        );
        assert!(require_may_create(Some(&actor(true)), true).is_ok());
    }
}
