//! sourcegull-services: the services shared by the web role's features, without HTTP routing.
//!
//! - [`AppState`]: handles shared by every feature (database, hot site config, metrics,
//!   readiness, authentication, repository permissions, repo node clients, email, blob
//!   storage, …). `sourcegull-http-api` routes requests to handlers that take it as their state.
//! - [`git`]: [`GitReader`](git::GitReader) reads repository contents from the repo node
//!   that owns a repository; the URL conventions of repository pages.
//! - [`email`]: outbound email (SMTP, or an in-process recorder in tests), password reset and
//!   email verification links.
//! - [`rate_limits`]: request limits per client address and account (`auth.rateLimits`).
//! - [`relay`]: opaque node IDs (`base64("User:123")`).
//! - [`search`]: search backends with code ownership read from the ownership store (search
//!   itself runs in `sourcegull-search-web`); [`compute`]: compute queries over search.
//! - [`batches`]: server-side bulk change spec execution, shared by GraphQL and the executor
//!   queue, and who may use bulk changes ([`batches::access`]); [`batch_files`]: where the files those specs mount are served.
//! - [`executor_secrets`]: who may manage executor secrets (their store is in `sourcegull-db`).
//! - [`saved_searches`] and [`search_contexts`]: who may see and change saved searches and
//!   search contexts (their stores are in `sourcegull-db`); [`settings`]: who may read and write
//!   a subject's settings.
//! - [`slow_log`]: the slow GraphQL request and search log.
//! - [`ci_job_tokens`]: verified GitLab CI job tokens of SCIP uploads.

pub mod batch_files;
pub mod batches;
pub mod ci_job_tokens;
pub mod compute;
pub mod email;
pub mod executor_secrets;
pub mod git;
pub mod rate_limits;
pub mod relay;
pub mod saved_searches;
pub mod search;
pub mod search_contexts;
pub mod settings;
pub mod slow_log;
mod state;

pub use search::search_backend;
pub use state::{AppState, Readiness, SiteConfigSource};
