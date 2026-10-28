//! The GraphQL API of executors: [`executors`] (the executors that sent a heartbeat
//! recently, site admins only) and [`executor_secrets`] (the secrets executor jobs read, with
//! their access logs).

// `crate::context` and `crate::pagination`, as in the schema assembly.
use sourcegull_graphql_core::{context, pagination};

pub mod executor_secrets;
pub mod executors;
mod feature;

pub use executor_secrets::{ExecutorSecretsMutation, ExecutorSecretsQuery, node_loaders};
pub use executors::ExecutorsQuery;
pub use feature::{ExecutorsFeatureQuery, ExecutorsGraphql};
