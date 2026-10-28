//! Per-schema services, per-request viewer, authorization helpers and error codes.

use std::sync::Arc;

use async_graphql::{Context, ErrorExtensions};
use sourcegull_authn::{AccessError, Actor, AuthError, Authenticator};
use sourcegull_config::SiteConfigHandle;
use sourcegull_crypto::Keyring;
use sourcegull_db::{Db, DbError};
use sourcegull_services::{AppState, SiteConfigSource};

/// Stable `extensions.code` values of GraphQL errors.
pub mod codes {
    /// The viewer is not signed in.
    pub const UNAUTHORIZED: &str = "UNAUTHORIZED";
    /// The viewer may not perform the operation.
    pub const FORBIDDEN: &str = "FORBIDDEN";
    /// A stale `lastID`: someone saved a newer version first.
    pub const CONFLICT: &str = "CONFLICT";
    /// The input failed validation; `extensions.messages` lists the problems.
    pub const VALIDATION: &str = "VALIDATION";
    /// The object comes from a configuration file (the site config, the global settings or the
    /// resources file) and cannot be edited through the API.
    pub const READ_ONLY: &str = "READ_ONLY";
    /// The referenced object does not exist.
    pub const NOT_FOUND: &str = "NOT_FOUND";
    /// A name is already taken.
    pub const ALREADY_EXISTS: &str = "ALREADY_EXISTS";
    /// Malformed input (e.g. an ID of the wrong kind).
    pub const INVALID_INPUT: &str = "INVALID_INPUT";
    /// An unexpected server-side failure.
    pub const INTERNAL: &str = "INTERNAL";
    /// The repository is not cloned on its repo node yet.
    pub const REPO_NOT_CLONED: &str = "REPO_NOT_CLONED";
    /// The repository's first clone is running.
    pub const CLONE_IN_PROGRESS: &str = "CLONE_IN_PROGRESS";
    /// A revision does not resolve to a commit.
    pub const REVISION_NOT_FOUND: &str = "REVISION_NOT_FOUND";
    /// A size or count limit was exceeded.
    pub const TOO_LARGE: &str = "TOO_LARGE";
    /// The repo node is unreachable or overloaded; retry later.
    pub const UNAVAILABLE: &str = "UNAVAILABLE";
}

/// Services shared by all resolvers (schema data). Cheap to clone.
#[derive(Debug, Clone)]
pub struct Services {
    /// The main database.
    pub db: Db,
    /// Authentication and access tokens.
    pub auth: Authenticator,
    /// The live site config.
    pub config: SiteConfigHandle,
    /// Where the site config comes from.
    pub site_config_source: SiteConfigSource,
    /// Data keys for secrets at rest; `None` when no key file is configured.
    pub keyring: Option<Arc<Keyring>>,
    /// Code host implementations by connection kind.
    pub registry: sourcegull_repos::Registry,
    /// gRPC clients of the repo nodes, routed by repo.
    pub repo_clients: Arc<sourcegull_rpc::RepoClients>,
    /// Repository permissions.
    pub authz: sourcegull_authz::Checker,
    /// The HTTP application state, for running searches; `None` in
    /// schemas built without one (search fields then fail).
    pub app: Option<AppState>,
}

impl Services {
    /// Takes the services from the HTTP application state.
    pub fn from_state(state: &AppState) -> Self {
        Self {
            db: state.db.clone(),
            auth: state.auth.clone(),
            config: state.config.clone(),
            site_config_source: state.site_config_source.clone(),
            keyring: state.keyring.clone(),
            registry: sourcegull_repos::Registry::default(),
            repo_clients: state.repo_clients.clone(),
            authz: state.authz.clone(),
            app: Some(state.clone()),
        }
    }
}

/// The caller of a request (request data). Absent or `Viewer(None)` means anonymous.
#[derive(Debug, Clone, Default)]
pub struct Viewer(pub Option<Actor>);

/// A GraphQL error with `extensions.code`.
pub fn error(code: &'static str, message: impl Into<String>) -> async_graphql::Error {
    async_graphql::Error::new(message).extend_with(|_, e| e.set("code", code))
}

/// Maps an [`AuthError`]; internal details are logged, not returned.
pub fn auth_error(e: AuthError) -> async_graphql::Error {
    match e {
        AuthError::Internal(source) => {
            tracing::error!(error = %source, "graphql: internal authentication error");
            error(codes::INTERNAL, "Internal error.")
        }
        AuthError::TokenInvalid | AuthError::InvalidCredentials => {
            error(codes::UNAUTHORIZED, e.to_string())
        }
        other => error(other.code(), other.to_string()),
    }
}

/// Maps an [`AccessError`] from a feature service; internal details are logged, not returned.
pub fn access_error(e: AccessError) -> async_graphql::Error {
    if let AccessError::Internal(source) = &e {
        tracing::error!(error = %source, "graphql: internal error checking access");
    }
    error(e.code(), e.to_string())
}

/// Maps a [`DbError`]; internal details are logged, not returned.
pub fn db_error(e: DbError) -> async_graphql::Error {
    match e {
        DbError::NotFound(what) => error(codes::NOT_FOUND, format!("{what} not found.")),
        DbError::AlreadyExists { .. } => error(codes::ALREADY_EXISTS, "The name is already taken."),
        DbError::Conflict { .. } => error(
            codes::CONFLICT,
            "The object was modified since it was loaded; reload and try again.",
        ),
        other => {
            tracing::error!(error = %other, "graphql: database error");
            error(codes::INTERNAL, "Internal error.")
        }
    }
}

/// The schema's [`Services`].
pub fn services<'a>(ctx: &Context<'a>) -> async_graphql::Result<&'a Services> {
    // `build_schema` always installs Services.
    ctx.data::<Services>()
}

/// The signed-in viewer, if any.
pub fn viewer<'a>(ctx: &Context<'a>) -> Option<&'a Actor> {
    ctx.data_opt::<Viewer>().and_then(|v| v.0.as_ref())
}

/// The repositories the viewer may read. Every resolver that loads repositories for
/// a viewer filters with it.
pub async fn repo_access(ctx: &Context<'_>) -> async_graphql::Result<sourcegull_types::RepoAccess> {
    services(ctx)?
        .authz
        .access(viewer(ctx))
        .await
        .map_err(db_error)
}

/// The signed-in viewer, or an `UNAUTHORIZED` error.
pub fn require_viewer<'a>(ctx: &Context<'a>) -> async_graphql::Result<&'a Actor> {
    sourcegull_authn::access::require_signed_in(viewer(ctx)).map_err(access_error)
}

/// The viewer if they are a site admin; `UNAUTHORIZED` / `FORBIDDEN` otherwise.
pub fn require_site_admin<'a>(ctx: &Context<'a>) -> async_graphql::Result<&'a Actor> {
    sourcegull_authn::access::require_site_admin(viewer(ctx)).map_err(access_error)
}

/// Clamps an optional `first:` argument to `1..=max`, defaulting to `default`.
pub fn page_size(first: Option<i32>, default: i64, max: i64) -> i64 {
    first.map_or(default, |n| i64::from(n).clamp(0, max))
}

/// Converts a count to a GraphQL `Int`, saturating.
pub fn to_int(n: i64) -> i32 {
    i32::try_from(n).unwrap_or(i32::MAX)
}

/// A `VALIDATION` error listing `messages` (also in `extensions.messages`).
pub fn validation(messages: Vec<String>) -> async_graphql::Error {
    let message = match messages.as_slice() {
        [one] => one.clone(),
        _ => format!(
            "The input is invalid:\n{}",
            messages
                .iter()
                .map(|m| format!("- {m}"))
                .collect::<Vec<_>>()
                .join("\n")
        ),
    };
    async_graphql::Error::new(message).extend_with(|_, ext| {
        ext.set("code", codes::VALIDATION);
        ext.set("messages", messages.clone());
    })
}
