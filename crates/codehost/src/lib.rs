//! Code host traits: the extension point for code hosts.
//!
//! Each code host crate (`codehost-gitlab`, `codehost-github`, `codehost-gitea`) implements
//! these traits; a registry keyed by [`ExternalServiceKind`](sourcegull_types::ExternalServiceKind)
//! (in `sourcegull-repos`) maps a connection to its implementations, so adding a host means one new
//! crate and one registry arm.
//!
//! - [`RepoSource`]: repository discovery.
//! - [`WebhookHandler`]: verifying and parsing incoming webhooks into [`DomainEvent`]s.
//! - [`WebhookRegistrar`]: registering a connection's webhooks on its repositories.
//! - [`AuthzProvider`]: repository permissions sync.
//! - [`ChangesetSource`]: merge requests for bulk changes.
//! - [`OAuthLoginProvider`]: sign-in with the code host.
//! - [`CiJobVerifier`]: which repository a CI job token belongs to.
//! - [`GroupSource`]: groups and their members, for syncing teams.
//! - [`ratelimit`]: the request budget every API client draws from.
//! - [`outbound`]: the outbound request log of API calls.
//! - [`http`]: client tuning and response header helpers shared by the REST clients.
//! - [`rest`]: the REST client the code host API clients are built on.

pub mod authz;
pub mod changeset;
mod ci;
mod error;
pub mod groups;
pub mod http;
pub mod oauth;
pub mod outbound;
pub mod ratelimit;
pub mod rest;
mod source;
mod webhook;

pub use authz::{AccountId, AuthzProvider, ExternalAccountData, UserPerms};
pub use changeset::{
    Changeset, ChangesetEvent, ChangesetEventKind, ChangesetSource, ChangesetState, CheckState,
    CommitAction, CommitFile, CreatedChangeset, MergeOptions, NewChangeset, NewCommit,
    PushStrategy, ReviewState,
};
pub use ci::{CiJob, CiJobVerifier};
pub use error::{CodeHostError, Result};
pub use groups::{CodeHostGroup, GroupMember, GroupSource};
pub use http::ClientOptions;
pub use oauth::{OAuthLoginProvider, OAuthToken, OAuthUser, parse_token_response};
pub use ratelimit::{
    InProcessLimiters, RateLimiter, RateLimiterState, RateLimiters, TokenBucket, connection_key,
};
pub use source::{CheckReport, Credential, ListOptions, RepoMeta, RepoSource, RepoStream};
pub use webhook::{
    ChangesetHook, ChecksHook, DomainEvent, HOOK_EVENT_PREFIX, HookChange, HookTarget, RefChange,
    RepoChange, WebhookError, WebhookHandler, WebhookRegistrar,
};
