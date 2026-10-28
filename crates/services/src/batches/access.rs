//! Who may use and manage bulk changes.
//!
//! - Bulk changes are available to signed-in users while `batchChanges.enabled` is on, and to
//!   site admins only with `batchChanges.restrictToAdmins` ([`available`]). Reading needs the
//!   `BATCH_CHANGES#READ` permission, changing `BATCH_CHANGES#WRITE`.
//! - Bulk changes and specs of a namespace are administered by whoever may act for it
//!   ([`can_act_for`]: the user, the org's members, site admins).
//! - A bulk change is closed or deleted by its creator or a site admin, or by any member of
//!   its organization when the org's settings set `orgs.allMembersBatchChangesAdmin`.
//! - A spec's execution is changed by its creator or a site admin; its logs and files are seen
//!   by them and by those who administer its namespace.
//! - Credentials of a user are managed by the user and site admins; site credentials by site
//!   admins.

use sourcegull_authn::access::{can_act_for, require_can_act_for};
use sourcegull_authn::{AccessError, Actor};
use sourcegull_batches::store::{BatchChange, BatchSpec, ListBatchChanges};
use sourcegull_config::SiteConfig;
use sourcegull_db::{Db, DbError, Namespace};
use sourcegull_types::{SettingsSubject, UserId};

/// Why bulk changes are not available to a signed-in user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unavailable {
    /// `batchChanges.enabled` is off.
    Disabled,
    /// `batchChanges.restrictToAdmins` is on and the user is not a site admin.
    DisabledForUser,
}

/// Whether bulk changes are available to `actor`.
pub fn available(config: &SiteConfig, actor: &Actor) -> Result<(), Unavailable> {
    if !config.batch_changes_enabled {
        return Err(Unavailable::Disabled);
    }
    if config.batch_changes_restrict_to_admins && !actor.site_admin {
        return Err(Unavailable::DisabledForUser);
    }
    Ok(())
}

/// Whether `actor` created a spec or changeset spec with creator `creator`, or is a site admin.
pub fn is_creator_or_site_admin(actor: &Actor, creator: Option<UserId>) -> bool {
    actor.site_admin || creator == Some(actor.user_id)
}

/// Whether `actor` (anonymous: `None`) may administer bulk changes and specs of `ns`.
pub async fn can_administer(
    db: &Db,
    actor: Option<&Actor>,
    ns: Namespace,
) -> Result<bool, DbError> {
    match actor {
        Some(actor) => can_act_for(db, actor, ns).await,
        None => Ok(false),
    }
}

/// Fails unless `actor` may create specs and bulk changes in `ns`: [`AccessError::NotFound`]
/// for a deleted or missing namespace, [`AccessError::Forbidden`] when they may not act for it.
pub async fn require_namespace_access(
    db: &Db,
    actor: &Actor,
    ns: Namespace,
) -> Result<(), AccessError> {
    if !namespace_exists(db, ns).await? {
        return Err(AccessError::NotFound("Namespace"));
    }
    require_can_act_for(db, actor, ns).await
}

async fn namespace_exists(db: &Db, ns: Namespace) -> Result<bool, DbError> {
    Ok(match ns {
        Namespace::User(id) => db.users().get_by_id(id).await?.is_some(),
        Namespace::Org(id) => db
            .orgs()
            .get(id)
            .await?
            .is_some_and(|o| o.deleted_at.is_none()),
    })
}

/// Fails with [`AccessError::Forbidden`] unless `actor` may close or delete `bc` (or run bulk
/// operations on its changesets).
pub async fn require_can_modify(
    db: &Db,
    actor: &Actor,
    bc: &BatchChange,
) -> Result<(), AccessError> {
    if is_creator_or_site_admin(actor, bc.creator_id) {
        return Ok(());
    }
    if let Namespace::Org(org) = bc.namespace
        && all_members_admin(db, org).await?
        && db.orgs().is_member(org, actor.user_id).await?
    {
        return Ok(());
    }
    Err(AccessError::forbidden(
        "Only the creator of the bulk change or a site admin may do this.",
    ))
}

/// Whether `org`'s settings set `orgs.allMembersBatchChangesAdmin`.
async fn all_members_admin(db: &Db, org: sourcegull_types::OrgId) -> Result<bool, DbError> {
    let settings = db.settings().latest(SettingsSubject::Org(org)).await?;
    Ok(settings
        .and_then(|s| sourcegull_config::parse_jsonc(&s.contents).ok())
        .and_then(|v| v.get("orgs.allMembersBatchChangesAdmin")?.as_bool())
        .unwrap_or(false))
}

/// Fails with [`AccessError::Forbidden`] unless `actor` may execute, cancel or retry `spec`.
pub fn require_can_modify_spec(actor: &Actor, spec: &BatchSpec) -> Result<(), AccessError> {
    if is_creator_or_site_admin(actor, spec.user_id) {
        Ok(())
    } else {
        Err(AccessError::forbidden(
            "Only the creator of the bulk change spec or a site admin may do this.",
        ))
    }
}

/// Whether `actor` (anonymous: `None`) may read `spec`'s execution logs and workspace files:
/// its creator, those who may administer its namespace, and site admins.
pub async fn can_see_spec_details(
    db: &Db,
    actor: Option<&Actor>,
    spec: &BatchSpec,
) -> Result<bool, DbError> {
    let Some(actor) = actor else {
        return Ok(false);
    };
    if is_creator_or_site_admin(actor, spec.user_id) {
        return Ok(true);
    }
    can_act_for(db, actor, spec.namespace).await
}

/// Narrows `opts` to the bulk changes `actor` may list: with `administered_only` those they
/// may administer; drafts only their own unless `include_drafts` (the bulk changes of one
/// changeset). Site admins see every bulk change.
pub fn scope_list(
    actor: &Actor,
    administered_only: bool,
    include_drafts: bool,
    opts: &mut ListBatchChanges,
) {
    if actor.site_admin {
        return;
    }
    if administered_only {
        opts.administered_by = Some(actor.user_id);
    }
    if !include_drafts {
        opts.exclude_drafts_not_owned_by = Some(actor.user_id);
    }
}

/// What is done with bulk change credentials, for the denial message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialAction {
    /// List the code hosts with the owner's credentials.
    ListCodeHosts,
    /// Look a credential up.
    View,
    /// Add a credential.
    Add,
    /// Delete a credential.
    Delete,
}

/// Fails with [`AccessError::Forbidden`] unless `actor` may `action` the credentials of
/// `user`: the user themself or a site admin. A user credential without a user (`None`) is
/// nobody's.
pub fn require_user_credentials(
    actor: &Actor,
    user: Option<UserId>,
    action: CredentialAction,
) -> Result<(), AccessError> {
    if user.is_some_and(|u| actor.is_self_or_site_admin(u)) {
        return Ok(());
    }
    Err(AccessError::forbidden(match action {
        CredentialAction::ListCodeHosts => {
            "Only the user and site admins may see the user's code hosts."
        }
        CredentialAction::View => "Only the user and site admins may see the user's credentials.",
        CredentialAction::Add => "Only the user and site admins may add the user's credentials.",
        CredentialAction::Delete => {
            "Only the user and site admins may delete the user's credentials."
        }
    }))
}

/// Fails with [`AccessError::Forbidden`] unless `actor` may `action` site credentials: site
/// admins only (listing the code hosts with site credentials is open to every user).
pub fn require_site_credentials(
    actor: &Actor,
    action: CredentialAction,
) -> Result<(), AccessError> {
    if actor.site_admin || action == CredentialAction::ListCodeHosts {
        return Ok(());
    }
    Err(AccessError::forbidden(match action {
        CredentialAction::Add => "Must be site admin to add a site credential.",
        _ => "Must be site admin to perform this action.",
    }))
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
    fn availability() {
        let user = actor(1, false);
        let admin = actor(2, true);
        let mut config = SiteConfig {
            batch_changes_enabled: true,
            ..Default::default()
        };
        assert_eq!(available(&config, &user), Ok(()));
        config.batch_changes_restrict_to_admins = true;
        assert_eq!(available(&config, &user), Err(Unavailable::DisabledForUser));
        assert_eq!(available(&config, &admin), Ok(()));
        config.batch_changes_enabled = false;
        assert_eq!(available(&config, &admin), Err(Unavailable::Disabled));
    }

    #[test]
    fn creator_rule() {
        let user = actor(1, false);
        assert!(is_creator_or_site_admin(&user, Some(UserId(1))));
        assert!(!is_creator_or_site_admin(&user, Some(UserId(2))));
        assert!(!is_creator_or_site_admin(&user, None));
        assert!(is_creator_or_site_admin(&actor(2, true), None));
    }

    #[test]
    fn list_scope() {
        let mut opts = ListBatchChanges::default();
        scope_list(&actor(1, true), true, false, &mut opts);
        assert_eq!(opts.administered_by, None);
        assert_eq!(opts.exclude_drafts_not_owned_by, None);
        scope_list(&actor(1, false), true, false, &mut opts);
        assert_eq!(opts.administered_by, Some(UserId(1)));
        assert_eq!(opts.exclude_drafts_not_owned_by, Some(UserId(1)));
        let mut opts = ListBatchChanges::default();
        scope_list(&actor(1, false), false, true, &mut opts);
        assert_eq!(opts.administered_by, None);
        assert_eq!(opts.exclude_drafts_not_owned_by, None);
    }

    #[test]
    fn credentials() {
        let user = actor(1, false);
        let admin = actor(2, true);
        assert!(require_user_credentials(&user, Some(UserId(1)), CredentialAction::Delete).is_ok());
        assert!(
            require_user_credentials(&admin, Some(UserId(1)), CredentialAction::Delete).is_ok()
        );
        let e =
            require_user_credentials(&user, Some(UserId(3)), CredentialAction::Add).unwrap_err();
        assert!(require_user_credentials(&admin, None, CredentialAction::Delete).is_err());
        assert_eq!(e.code(), "FORBIDDEN");
        assert_eq!(
            e.to_string(),
            "Only the user and site admins may add the user's credentials."
        );
        assert!(require_site_credentials(&user, CredentialAction::ListCodeHosts).is_ok());
        assert!(require_site_credentials(&admin, CredentialAction::Add).is_ok());
        assert_eq!(
            require_site_credentials(&user, CredentialAction::Add)
                .unwrap_err()
                .to_string(),
            "Must be site admin to add a site credential."
        );
        assert_eq!(
            require_site_credentials(&user, CredentialAction::Delete)
                .unwrap_err()
                .to_string(),
            "Must be site admin to perform this action."
        );
    }
}
