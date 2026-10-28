//! Who may see and change saved searches.
//!
//! Secret saved searches are visible to whoever may act for the owner
//! ([`can_act_for`]: the user, the org's members, site admins); public ones to every reader.
//! Writing needs the same; making one public needs a site admin. Listing every saved search
//! needs a site admin too.

use sourcegull_authn::access::{can_act_for, require_can_act_for, require_signed_in};
use sourcegull_authn::{AccessError, Actor};
use sourcegull_db::{
    Db, DbError, ListSavedSearchesOptions, Namespace, SavedSearch, SavedSearchVisibility,
};

/// Whether `actor` (anonymous: `None`) may see `s`.
pub async fn can_view(db: &Db, actor: Option<&Actor>, s: &SavedSearch) -> Result<bool, DbError> {
    if s.visibility == SavedSearchVisibility::Public {
        return Ok(true);
    }
    can_administer(db, actor, s).await
}

/// Whether `actor` (anonymous: `None`) may edit, transfer or delete `s`.
pub async fn can_administer(
    db: &Db,
    actor: Option<&Actor>,
    s: &SavedSearch,
) -> Result<bool, DbError> {
    match actor {
        Some(actor) => can_act_for(db, actor, s.owner).await,
        None => Ok(false),
    }
}

/// Fails unless `actor` may administer `s`: [`AccessError::NotFound`] when they may not even
/// see it, [`AccessError::Forbidden`] when they may only see it.
pub async fn require_can_administer(
    db: &Db,
    actor: &Actor,
    s: &SavedSearch,
) -> Result<(), AccessError> {
    if !can_view(db, Some(actor), s).await? {
        return Err(not_found());
    }
    require_can_act_for(db, actor, s.owner).await
}

/// Fails with [`AccessError::Forbidden`] unless `actor` may own saved searches in `ns`
/// (create them there, transfer them there).
pub async fn require_can_own(db: &Db, actor: &Actor, ns: Namespace) -> Result<(), AccessError> {
    require_can_act_for(db, actor, ns).await
}

/// The error for a saved search that is missing or that the actor may not see.
pub fn not_found() -> AccessError {
    AccessError::NotFound("Saved search")
}

/// Fails with [`AccessError::Forbidden`] unless `actor` may make saved searches public (or
/// secret again): site admins only.
pub fn require_publisher(actor: &Actor) -> Result<(), AccessError> {
    if actor.site_admin {
        Ok(())
    } else {
        Err(AccessError::forbidden(
            "Only site admins can make saved searches public.",
        ))
    }
}

/// Narrows `opts` to what `actor` may list: the saved searches of `owner` (which needs the
/// right to act for it), or with `viewer_is_affiliated` the actor's, their orgs' and public
/// ones (anonymous: public ones). Listing all of them needs a site admin.
pub async fn scope_list(
    db: &Db,
    actor: Option<&Actor>,
    owner: Option<Namespace>,
    viewer_is_affiliated: bool,
    opts: &mut ListSavedSearchesOptions,
) -> Result<(), AccessError> {
    if let Some(ns) = owner {
        let actor = require_signed_in(actor)?;
        require_can_act_for(db, actor, ns).await?;
        opts.owner = Some(ns);
    }
    if viewer_is_affiliated {
        match actor {
            Some(a) => opts.affiliated_user = Some(a.user_id),
            None => opts.public_only = true,
        }
    }
    check_list_all(actor, opts)
}

/// Fails unless `opts` is narrowed (owner, affiliated user or public only) or `actor` is a
/// site admin.
fn check_list_all(
    actor: Option<&Actor>,
    opts: &ListSavedSearchesOptions,
) -> Result<(), AccessError> {
    if opts.owner.is_none()
        && opts.affiliated_user.is_none()
        && !opts.public_only
        && !actor.is_some_and(|a| a.site_admin)
    {
        return Err(AccessError::forbidden(
            "Must be site admin to list all saved searches; pass owner or viewerIsAffiliated.",
        ));
    }
    Ok(())
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
    fn publishing_needs_a_site_admin() {
        assert!(require_publisher(&actor(1, true)).is_ok());
        let e = require_publisher(&actor(1, false)).unwrap_err();
        assert_eq!(e.code(), "FORBIDDEN");
    }

    #[test]
    fn listing_everything_needs_a_site_admin() {
        let all = ListSavedSearchesOptions::default();
        assert!(check_list_all(Some(&actor(1, true)), &all).is_ok());
        assert_eq!(
            check_list_all(Some(&actor(1, false)), &all)
                .unwrap_err()
                .code(),
            "FORBIDDEN"
        );
        assert!(check_list_all(None, &all).is_err());
        let public = ListSavedSearchesOptions {
            public_only: true,
            ..Default::default()
        };
        assert!(check_list_all(None, &public).is_ok());
        let mine = ListSavedSearchesOptions {
            affiliated_user: Some(UserId(1)),
            ..Default::default()
        };
        assert!(check_list_all(Some(&actor(1, false)), &mine).is_ok());
    }

    #[test]
    fn hidden_saved_searches_are_not_found() {
        assert_eq!(not_found().to_string(), "Saved search not found.");
    }
}
