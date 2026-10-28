//! sourcegull-rpc: gRPC definitions shared by the roles.
//!
//! - [`repo::v1`]: code generated from `proto/repo.proto`, the repo node service.
//!   The server lives in `sourcegull-gitstore`; this crate only has the
//!   messages, the client stub and the server trait.
//! - [`ErrorKind`]: typed error kinds carried in status metadata.
//! - [`Router`]: repo → node assignment by rendezvous hashing.
//! - [`RepoClients`]: one cached channel per node, routed by [`Router`].
//! - [`incoming`]: a listener's connections for tonic, with `TCP_NODELAY`.
//! - [`request_deadline`]: a call's deadline from its `grpc-timeout`.
//!
//! The protos are compiled with `protox` at build time, so `protoc` is not needed.

mod client;
mod error_kind;
mod routing;
mod server;

pub use client::{ClientConfig, ClientError, RepoClients, connect_lazy};
pub use error_kind::{ERROR_KIND_METADATA_KEY, ErrorKind, UnknownErrorKind};
pub use routing::Router;
pub use server::{incoming, request_deadline};

/// Generated protobuf and gRPC code.
pub mod repo {
    /// Version 1 of the repo node service (`sourcegull.repo.v1`).
    pub mod v1 {
        #![allow(missing_docs, clippy::all, clippy::pedantic)]
        tonic::include_proto!("sourcegull.repo.v1");

        impl std::fmt::Debug for PushCredentials {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.debug_struct("PushCredentials")
                    .field("username", &self.username)
                    .field("token", &"[redacted]")
                    .finish()
            }
        }
    }
}

pub use tonic;
