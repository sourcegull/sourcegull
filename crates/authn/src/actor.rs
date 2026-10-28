//! [`Actor`]: who is making a request, and how they authenticated.

use crate::{Permission, PermissionSet};
use sourcegull_types::{AccessTokenId, UserId};

/// Access token scope granting full access as the token's user.
pub const SCOPE_USER_ALL: &str = "user:all";
/// Access token scope allowing a site admin's token to act as any user
/// (`Authorization: token-sudo user="…",token="…"`).
pub const SCOPE_SITE_ADMIN_SUDO: &str = "site-admin:sudo";
/// Every scope a token may carry.
pub const KNOWN_SCOPES: [&str; 2] = [SCOPE_USER_ALL, SCOPE_SITE_ADMIN_SUDO];

/// How an [`Actor`] authenticated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthVia {
    /// A session cookie.
    Session,
    /// A personal access token.
    Token {
        /// The token used.
        id: AccessTokenId,
        /// Its scopes.
        scopes: Vec<String>,
    },
    /// A site admin's `site-admin:sudo` token acting as another user.
    Sudo {
        /// The token used.
        token_id: AccessTokenId,
        /// The site admin who owns the token.
        by_user_id: UserId,
    },
    /// An OAuth access token issued to an MCP client, only accepted by the resource it is
    /// bound to.
    OAuth {
        /// The grant the token belongs to.
        grant_id: i64,
        /// The client's `client_id`.
        client_id: String,
        /// Its scopes.
        scopes: Vec<String>,
    },
}

/// An authenticated user, with their RBAC permissions resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Actor {
    /// The user the request acts as (the impersonated user for sudo).
    pub user_id: UserId,
    /// Their username.
    pub username: String,
    /// Whether they are a site admin.
    pub site_admin: bool,
    /// How they authenticated.
    pub via: AuthVia,
    /// Their RBAC permissions.
    pub permissions: PermissionSet,
}

impl Actor {
    /// Whether the actor holds permission `p`.
    pub fn has(&self, p: Permission) -> bool {
        self.permissions.has(p)
    }

    /// Whether the actor may act on resources owned by `user` (self or site admin).
    pub fn is_self_or_site_admin(&self, user: UserId) -> bool {
        self.site_admin || self.user_id == user
    }
}
