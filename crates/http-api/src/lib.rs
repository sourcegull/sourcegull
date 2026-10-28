//! sourcegull-http-api: the HTTP plumbing shared by every role.
//!
//! Handlers take `sourcegull_services::AppState` as their state; the extractors and error
//! responses they share are in `sourcegull-http-extract`.
//!
//! - [`public_routes`] / [`internal_routes`]: the route sets of the public (`:7080`) and
//!   internal (`:6060`) listeners. Other crates (GraphQL, auth) `.merge()` their own
//!   `Router<AppState>` into [`public_routes`] and then call [`finish`], which applies the
//!   shared middleware (request id, tracing, security headers, gzip, metrics, body limit).
//! - [`authenticate`] (applied by [`finish`]): resolves the caller from an access token or the
//!   session cookie and enforces the CSRF rule; [`auth_routes`]: `/-/sign-up`, `/-/sign-in`,
//!   `/-/sign-out`, `/-/auth-state`, GitLab OAuth and OpenID Connect sign-in.
//! - Streaming search (`/.api/search/stream`, [`search`]) and compute
//!   (`/.api/compute/stream`, [`compute`]) over `sourcegull-search-web`.
//! - Code host webhooks (`/.api/gitlab-webhooks`, `/.api/webhooks/{uuid}`), part of
//!   [`public_routes`] and handled by `sourcegull_repos::WebhookIngest`.
//! - Repository content: raw files and directory archives
//!   (`/{repo}[@{rev}]/-/raw/{path}`) and streaming blame (`/.api/blame/…`), see
//!   [`repo_content`].
//! - Executors: the queue API, job tokens and git checkouts under `/.executors/*`, see
//!   [`executors`]; files server-side bulk change specs mount (`/.api/files/batch-changes/…`,
//!   `/.executors/files/batch-changes/…`) in [`batch_files`].
//! - SCIM 2.0 user provisioning (`/.api/scim/v2/*`), see [`scim`].
//! - [`serve`]: runs a router with graceful shutdown on a `CancellationToken`.

mod assets;
mod auth;
mod auth_routes;
pub mod batch_files;
pub mod ci_job_auth;
pub mod compute;
pub mod executors;
mod health;
mod oauth_routes;
mod oidc_routes;
pub mod repo_content;
mod router;
pub mod scim;
pub mod scip_upload;
pub mod search;
mod serve;
mod webhooks;

pub use assets::{CACHE_IMMUTABLE, CACHE_REVALIDATE, content_security_policy};
pub use auth::{authenticate, session_cookie};
pub use auth_routes::auth_routes;
pub use health::READYZ_DB_TIMEOUT;
pub use oauth_routes::{FLOW_TTL, STATE_COOKIE};
pub use router::{
    DEFAULT_BODY_LIMIT, HTTP_DURATION_METRIC, HTTP_REQUESTS_METRIC, REQUEST_ID_HEADER, finish,
    internal_router, internal_routes, public_router, public_routes,
};
pub use serve::{DRAIN_TIMEOUT, bind, serve};
