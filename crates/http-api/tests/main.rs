//! The integration tests: one module per file, linked into a single test binary.
//! A new file under `tests/` must be declared here.

mod auth;
mod batch_files;
mod batches_queue;
mod executors;
mod oauth;
mod oauth_hosts;
mod oidc;
mod own_search;
mod rate_limits;
mod repo_content;
mod repo_predicates;
mod router;
mod scim;
mod scip_upload;
mod search_limits;
mod webhooks;
