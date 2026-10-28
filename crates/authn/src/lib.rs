//! Authentication for Sourcegull: built-in username/password
//! accounts, sessions, personal access tokens and RBAC permission sets.
//!
//! - [`Authenticator`]: sign-up (first user becomes site admin), sign-in with
//!   lockout, sign-out, session and access-token authentication, token management,
//!   and sign-in through an external provider ([`ExternalIdentity`]).
//! - [`SessionStore`]: the session storage trait, with
//!   [`PgSessionStore`] (Postgres + short in-process cache) and
//!   [`MemorySessionStore`].
//! - [`Actor`]: the authenticated user with a [`PermissionSet`] bitset.
//! - [`AuthError`]: errors with stable machine codes.
//! - [`access`]: [`AccessError`] and the namespace rule, shared by every API.

pub mod access;
mod actor;
mod authenticator;
mod error;
mod external;
mod header;
pub mod oauth;
mod permissions;
mod session;
pub mod validate;

pub use access::AccessError;
pub use actor::{Actor, AuthVia, KNOWN_SCOPES, SCOPE_SITE_ADMIN_SUDO, SCOPE_USER_ALL};
pub use authenticator::{Authenticator, client_address, events};
pub use error::{AuthError, BoxError};
pub use external::{ExternalIdentity, ExternalSignIn};
pub use header::{TokenCredential, parse_authorization};
pub use permissions::{Permission, PermissionSet};
pub use session::{
    DEFAULT_SESSION_CACHE_TTL, MemorySessionStore, PgSessionStore, Session, SessionStore,
    SessionToken,
};
pub use sourcegull_crypto::password::PasswordParams;
