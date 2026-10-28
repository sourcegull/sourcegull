//! Node kinds and ID parsers of the shared types.

use async_graphql::Result;
use sourcegull_db::Namespace;
use sourcegull_services::relay::{unmarshal_id, unmarshal_int_id};
use sourcegull_types::{OrgId, RepoId, UserId};

use crate::context::{codes, error};

/// Node kind of user IDs.
pub const USER_KIND: &str = "User";
/// Node kind of organization IDs.
pub const ORG_KIND: &str = "Org";
/// Node kind of repository IDs.
pub const REPOSITORY_KIND: &str = "Repository";

/// Parses a `User` node ID.
pub fn parse_user_id(id: &str) -> Result<UserId> {
    unmarshal_int_id(USER_KIND, id)
        .map(UserId)
        .ok_or_else(|| error(codes::INVALID_INPUT, format!("Invalid user ID {id:?}.")))
}

/// Parses a `Repository` node ID.
pub fn parse_repository_id(id: &str) -> Result<RepoId> {
    unmarshal_int_id(REPOSITORY_KIND, id)
        .and_then(|n| i32::try_from(n).ok())
        .map(RepoId)
        .ok_or_else(|| {
            error(
                codes::INVALID_INPUT,
                format!("Invalid repository ID {id:?}."),
            )
        })
}

/// Parses a `User` or `Org` node ID.
pub fn parse_namespace_id(id: &str) -> Result<Namespace> {
    let invalid = || {
        error(
            codes::INVALID_INPUT,
            format!("Invalid namespace ID {id:?}."),
        )
    };
    let (kind, key) = unmarshal_id(id).ok_or_else(invalid)?;
    let key: i64 = key.parse().map_err(|_| invalid())?;
    match kind.as_str() {
        USER_KIND => Ok(Namespace::User(UserId(key))),
        ORG_KIND => Ok(Namespace::Org(OrgId(key))),
        _ => Err(invalid()),
    }
}
