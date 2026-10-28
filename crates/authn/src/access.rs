//! Access decisions shared by every API. Feature services take the [`Actor`] and return
//! [`AccessError`]; GraphQL, MCP and the HTTP routes only map it to their error form.
//!
//! The namespace rule lives here too: who may act for a user or an organization
//! ([`can_act_for`]).

use std::borrow::Cow;

use sourcegull_db::{Db, DbError, Namespace};

use crate::error::BoxError;
use crate::{Actor, Permission};

/// Why an operation was refused, or the failure that stopped the check.
#[derive(Debug, thiserror::Error)]
pub enum AccessError {
    /// The operation needs a signed-in user.
    #[error("You must be signed in.")]
    SignInRequired,
    /// The actor may not perform the operation.
    #[error("{0}")]
    Forbidden(Cow<'static, str>),
    /// The object does not exist, or the actor may not see it; the two are not told apart.
    #[error("{0} not found.")]
    NotFound(&'static str),
    /// The check itself failed (database…); the details are logged by the API layer.
    #[error("Internal error.")]
    Internal(#[source] BoxError),
}

impl AccessError {
    /// A [`Forbidden`](AccessError::Forbidden) error.
    pub fn forbidden(msg: impl Into<Cow<'static, str>>) -> Self {
        AccessError::Forbidden(msg.into())
    }

    /// Stable machine code: `UNAUTHORIZED`, `FORBIDDEN`, `NOT_FOUND` or `INTERNAL`.
    pub fn code(&self) -> &'static str {
        match self {
            AccessError::SignInRequired => "UNAUTHORIZED",
            AccessError::Forbidden(_) => "FORBIDDEN",
            AccessError::NotFound(_) => "NOT_FOUND",
            AccessError::Internal(_) => "INTERNAL",
        }
    }

    /// Suggested HTTP status code.
    pub fn http_status(&self) -> u16 {
        match self {
            AccessError::SignInRequired => 401,
            AccessError::Forbidden(_) => 403,
            AccessError::NotFound(_) => 404,
            AccessError::Internal(_) => 500,
        }
    }
}

impl From<DbError> for AccessError {
    fn from(e: DbError) -> Self {
        AccessError::Internal(Box::new(e))
    }
}

/// The signed-in actor, or [`AccessError::SignInRequired`].
pub fn require_signed_in(actor: Option<&Actor>) -> Result<&Actor, AccessError> {
    actor.ok_or(AccessError::SignInRequired)
}

/// The actor if they are a site admin.
pub fn require_site_admin(actor: Option<&Actor>) -> Result<&Actor, AccessError> {
    let actor = require_signed_in(actor)?;
    if !actor.site_admin {
        return Err(AccessError::forbidden(
            "Must be site admin to perform this action.",
        ));
    }
    Ok(actor)
}

/// Fails with [`AccessError::Forbidden`] unless `actor`'s roles grant `p`.
pub fn require_permission(actor: &Actor, p: Permission) -> Result<(), AccessError> {
    if actor.has(p) {
        Ok(())
    } else {
        Err(AccessError::forbidden(format!(
            "You are missing the {p} permission."
        )))
    }
}

/// Whether `actor` may act for `ns`: the user themself, a member of the live organization, or
/// a site admin.
pub async fn can_act_for(db: &Db, actor: &Actor, ns: Namespace) -> Result<bool, DbError> {
    if actor.site_admin {
        return Ok(true);
    }
    Ok(match ns {
        Namespace::User(u) => actor.user_id == u,
        Namespace::Org(o) => db.orgs().is_member(o, actor.user_id).await?,
    })
}

/// Fails with [`AccessError::Forbidden`] unless `actor` may act for `ns`.
pub async fn require_can_act_for(db: &Db, actor: &Actor, ns: Namespace) -> Result<(), AccessError> {
    if can_act_for(db, actor, ns).await? {
        Ok(())
    } else {
        Err(AccessError::forbidden(
            "You may not act for this user or organization.",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_and_statuses() {
        assert_eq!(AccessError::SignInRequired.http_status(), 401);
        assert_eq!(AccessError::forbidden("no").code(), "FORBIDDEN");
        assert_eq!(AccessError::NotFound("Team").to_string(), "Team not found.");
        let e = AccessError::from(DbError::Invalid("x"));
        assert_eq!(e.to_string(), "Internal error.");
        assert_eq!(e.http_status(), 500);
    }

    #[test]
    fn missing_permission() {
        let actor = Actor {
            user_id: sourcegull_types::UserId(1),
            username: "u".to_owned(),
            site_admin: true,
            via: crate::AuthVia::Session,
            permissions: crate::PermissionSet::default(),
        };
        let e = require_permission(&actor, Permission::BatchChangesWrite).unwrap_err();
        assert_eq!(e.code(), "FORBIDDEN");
        assert_eq!(
            e.to_string(),
            "You are missing the BATCH_CHANGES#WRITE permission."
        );
    }
}
