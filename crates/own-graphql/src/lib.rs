//! Code Ownership over GraphQL, on top of `sourcegull-own`: owners of files and directories,
//! ingested CODEOWNERS files, assignments, signals and coverage ([`ownership`]), and teams
//! ([`teams`]), which CODEOWNERS owners resolve to and which can be assigned as owners.

mod feature;
pub mod ownership;
pub mod teams;

// `crate::context` and `crate::pagination`, as in the schema assembly.
use sourcegull_graphql_core::{context, pagination};

use async_graphql::Context;
use sourcegull_graphql_core::node::{NodeFuture, NodeLoader};
use sourcegull_services::relay::unmarshal_int_id;

pub use feature::{OwnFeatureMutation, OwnFeatureQuery, OwnGraphql};
pub use ownership::{OwnMutation, OwnQuery};
pub use teams::{TEAM_KIND, TeamConnection, TeamNode, TeamsMutation, TeamsQuery};

/// The `node(id:)` loader of teams, for the schema assembly's node type `N`.
pub fn node_loaders<N: From<TeamNode> + Send + 'static>() -> [NodeLoader<N>; 1] {
    fn load<'a, N: From<TeamNode> + Send + 'static>(
        ctx: &'a Context<'_>,
        _kind: &'a str,
        id: &'a str,
    ) -> NodeFuture<'a, N> {
        Box::pin(async move {
            let Some(key) = unmarshal_int_id(TEAM_KIND, id) else {
                return Ok(None);
            };
            Ok(teams::load_node(ctx, key).await?.map(N::from))
        })
    }
    [NodeLoader {
        kinds: &[TEAM_KIND],
        load: load::<N>,
    }]
}
