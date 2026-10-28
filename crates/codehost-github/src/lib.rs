//! The GitHub code host (github.com and GitHub Enterprise Server).
//!
//! - [`config`]: the connection configuration, with validation and secret redaction.
//! - [`app`]: GitHub App installation tokens for connections with `githubApp`.
//! - [`client`]: the REST API client: rate limiting, retries, pagination.
//! - [`source`]: repository discovery, implementing
//!   [`RepoSource`](sourcegull_codehost::RepoSource).
//! - [`webhooks`]: webhook verification and parsing, implementing
//!   [`WebhookHandler`](sourcegull_codehost::WebhookHandler).
//! - [`oauth`]: sign-in with GitHub OAuth, implementing
//!   [`OAuthLoginProvider`](sourcegull_codehost::OAuthLoginProvider).
//! - [`authz`]: repository permissions, implementing
//!   [`AuthzProvider`](sourcegull_codehost::AuthzProvider) (connections with an `authorization`
//!   block; otherwise every Sourcegull user can see every repository of the connection).
//! - `testing` (feature `testing`): a mock GitHub API server for tests.
//!
//! Pull requests for bulk changes, CI uploads and team sync are not implemented for GitHub.

pub mod app;
pub mod authz;
pub mod client;
pub mod config;
pub mod oauth;
pub mod source;
#[cfg(feature = "testing")]
pub mod testing;
pub mod types;
pub mod webhooks;

pub use authz::GitHubAuthzProvider;
pub use client::GitHubClient;
pub use config::{GitHubConnection, ValidatedConnection, connection_schema, validate_connection};
pub use oauth::GitHubOAuth;
pub use source::{GitHubSource, SERVICE_TYPE};
pub use webhooks::GitHubWebhooks;
