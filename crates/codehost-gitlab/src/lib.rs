//! The GitLab code host (gitlab.com and self-managed).
//!
//! - [`config`]: the connection configuration, in the `gitlab.schema.json`
//!   shape, with validation and secret redaction.
//! - [`client`]: the REST API client: rate limiting, retries, pagination, version
//!   detection.
//! - [`source`]: repository discovery, implementing
//!   [`RepoSource`](sourcegull_codehost::RepoSource).
//! - [`authz`]: repository permissions, implementing
//!   [`AuthzProvider`](sourcegull_codehost::AuthzProvider).
//! - [`ci`]: CI job tokens, implementing [`CiJobVerifier`](sourcegull_codehost::CiJobVerifier).
//! - [`oauth`]: sign-in with GitLab, implementing
//!   [`OAuthLoginProvider`](sourcegull_codehost::OAuthLoginProvider).
//! - [`changesets`]: merge requests for bulk changes, implementing
//!   [`ChangesetSource`](sourcegull_codehost::ChangesetSource).
//! - [`groups`]: groups and their members, implementing
//!   [`GroupSource`](sourcegull_codehost::GroupSource).
//! - [`webhooks`]: webhook verification and parsing, implementing
//!   [`WebhookHandler`](sourcegull_codehost::WebhookHandler).
//! - [`hooks`]: project webhooks registered through the API, implementing
//!   [`WebhookRegistrar`](sourcegull_codehost::WebhookRegistrar).
//! - [`token`]: OAuth connection tokens refreshed before expiry and after a 401.
//! - `testing` (feature `testing`): a mock GitLab API server for tests.

pub mod authz;
pub mod changesets;
pub mod ci;
pub mod client;
pub mod config;
pub mod groups;
pub mod hooks;
pub mod oauth;
pub mod source;
#[cfg(feature = "testing")]
pub mod testing;
pub mod token;
pub mod types;
pub mod webhooks;

pub use authz::GitLabAuthzProvider;
pub use changesets::{ChangesetOptions, GitLabChangesetSource};
pub use ci::GitLabCiJobs;
pub use client::{Auth, ClientOptions, GitLabClient};
pub use config::{GitLabConnection, ValidatedConnection, connection_schema, validate_connection};
pub use groups::GitLabGroups;
pub use hooks::GitLabHooks;
pub use oauth::GitLabOAuth;
pub use source::{GitLabSource, SERVICE_TYPE};
pub use token::{OAuthTokens, TokenRefresher, TokenSession};
pub use webhooks::GitLabWebhooks;
