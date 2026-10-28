//! The Gitea code host (self-hosted Gitea instances and gitea.com).
//!
//! - [`config`]: the connection configuration, with validation and secret redaction.
//! - [`client`]: the REST API client: rate limiting, retries, pagination.
//! - [`source`]: repository discovery, implementing
//!   [`RepoSource`](sourcegull_codehost::RepoSource).
//! - [`webhooks`]: webhook verification and parsing, implementing
//!   [`WebhookHandler`](sourcegull_codehost::WebhookHandler).
//! - [`oauth`]: sign-in with Gitea OAuth, implementing
//!   [`OAuthLoginProvider`](sourcegull_codehost::OAuthLoginProvider).
//! - [`authz`]: repository permissions, implementing
//!   [`AuthzProvider`](sourcegull_codehost::AuthzProvider), for connections with an
//!   `authorization` block (without one, every Sourcegull user can see every repository of the
//!   connection).
//! - `testing` (feature `testing`): a mock Gitea API server for tests.
//!
//! Pull requests for bulk changes, CI uploads and team sync are not implemented for Gitea.

pub mod authz;
pub mod client;
pub mod config;
pub mod oauth;
pub mod source;
#[cfg(feature = "testing")]
pub mod testing;
pub mod types;
pub mod webhooks;

pub use authz::GiteaAuthzProvider;
pub use client::GiteaClient;
pub use config::{GiteaConnection, ValidatedConnection, connection_schema, validate_connection};
pub use oauth::GiteaOAuth;
pub use source::{GiteaSource, SERVICE_TYPE};
pub use webhooks::GiteaWebhooks;
