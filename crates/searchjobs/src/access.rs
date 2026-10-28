//! Who may see and manage search jobs: a job's creator and site admins. The GraphQL API and
//! the download routes ([`crate::http`]) decide through these functions.

use sourcegull_authn::Actor;
use sourcegull_authn::access::AccessError;
use sourcegull_types::UserId;

use crate::store::{SearchJob, SearchJobStore};

/// The message for a job the actor may not access.
pub const FORBIDDEN: &str = "Only the creator of a search job and site admins may access it.";

/// Whether `actor` may see and manage a job created by `initiator`: its creator and site
/// admins.
pub fn can_access(actor: &Actor, initiator: UserId) -> bool {
    actor.is_self_or_site_admin(initiator)
}

/// Fails with [`AccessError::Forbidden`] unless `actor` may access a job created by
/// `initiator`.
pub fn require_access(actor: &Actor, initiator: UserId) -> Result<(), AccessError> {
    if can_access(actor, initiator) {
        Ok(())
    } else {
        Err(AccessError::forbidden(FORBIDDEN))
    }
}

/// The job `id` if `actor` may access it.
pub async fn job_for(
    store: &SearchJobStore,
    actor: &Actor,
    id: i64,
) -> Result<SearchJob, AccessError> {
    let job = store
        .get(id)
        .await?
        .ok_or(AccessError::NotFound("Search job"))?;
    // 🚨 SECURITY: only the creator and site admins may see or manage a job.
    require_access(actor, job.initiator_id)?;
    Ok(job)
}

/// Fails unless job `id` exists and `actor` may access it, reading only its creator.
pub async fn require_job_access(
    store: &SearchJobStore,
    actor: &Actor,
    id: i64,
) -> Result<(), AccessError> {
    let initiator = store
        .initiator(id)
        .await?
        .ok_or(AccessError::NotFound("Search job"))?;
    // 🚨 SECURITY: only the creator and site admins may see or manage a job.
    require_access(actor, initiator)
}

/// The creators whose jobs `actor` lists when asking for `requested` (empty: no filter):
/// users list their own jobs; only site admins list or filter by others'. `None` is every
/// creator.
pub fn list_initiators(
    actor: &Actor,
    requested: Vec<UserId>,
) -> Result<Option<Vec<UserId>>, AccessError> {
    if actor.site_admin {
        Ok((!requested.is_empty()).then_some(requested))
    } else if requested.iter().any(|id| *id != actor.user_id) {
        Err(AccessError::forbidden(
            "Only site admins may list other users' search jobs.",
        ))
    } else {
        Ok(Some(vec![actor.user_id]))
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
    fn creator_and_site_admins_access_a_job() {
        assert!(require_access(&actor(1, false), UserId(1)).is_ok());
        assert!(require_access(&actor(2, true), UserId(1)).is_ok());
        let e = require_access(&actor(2, false), UserId(1)).unwrap_err();
        assert_eq!(e.code(), "FORBIDDEN");
        assert_eq!(e.to_string(), FORBIDDEN);
    }

    #[test]
    fn only_site_admins_list_others_jobs() {
        let user = actor(1, false);
        assert_eq!(
            list_initiators(&user, vec![]).unwrap(),
            Some(vec![UserId(1)])
        );
        assert_eq!(
            list_initiators(&user, vec![UserId(1)]).unwrap(),
            Some(vec![UserId(1)])
        );
        assert_eq!(
            list_initiators(&user, vec![UserId(2)]).unwrap_err().code(),
            "FORBIDDEN"
        );
        let admin = actor(9, true);
        assert_eq!(list_initiators(&admin, vec![]).unwrap(), None);
        assert_eq!(
            list_initiators(&admin, vec![UserId(2)]).unwrap(),
            Some(vec![UserId(2)])
        );
    }
}
