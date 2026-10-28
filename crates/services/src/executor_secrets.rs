//! Who may manage executor secrets.
//!
//! Global secrets (no namespace) are managed by site admins; a namespace's secrets by whoever
//! may act for it ([`can_act_for`]: the user, the org's members, site admins). Seeing a secret
//! (without its value) or its access log needs the same.

use sourcegull_authn::access::{can_act_for, require_signed_in, require_site_admin};
use sourcegull_authn::{AccessError, Actor};
use sourcegull_db::{Db, Namespace};

/// Fails unless `actor` (anonymous: `None`) may manage the secrets of `ns` (global: `None`).
pub async fn require_can_manage<'a>(
    db: &Db,
    actor: Option<&'a Actor>,
    ns: Option<Namespace>,
) -> Result<&'a Actor, AccessError> {
    let actor = require_signed_in(actor)?;
    let Some(ns) = ns else {
        return require_site_admin(Some(actor));
    };
    if can_act_for(db, actor, ns).await? {
        Ok(actor)
    } else {
        Err(AccessError::forbidden(
            "You may not access the executor secrets of this user or organization.",
        ))
    }
}
