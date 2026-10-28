//! Code Ownership and teams in the schema.

use async_graphql::registry::Registry;
use async_graphql::{Context, MergedObject};
use sourcegull_graphql_core::feature::GraphqlFeature;
use sourcegull_graphql_core::node::{Node, NodeFuture, NodeLoader, NodeRegistry, register_node};
use sourcegull_services::relay::unmarshal_int_id;

use crate::ownership::{CODEOWNERS_FILE_KIND, CodeownersIngestedFileNode};
use crate::{OwnMutation, OwnQuery, TeamNode, TeamsMutation, TeamsQuery, node_loaders, ownership};

/// The GraphQL feature of Code Ownership and teams.
pub struct OwnGraphql;

/// The root query fields of Code Ownership and teams.
#[derive(MergedObject, Default)]
pub struct OwnFeatureQuery(TeamsQuery, OwnQuery);

/// The root mutation fields of Code Ownership and teams.
#[derive(MergedObject, Default)]
pub struct OwnFeatureMutation(TeamsMutation, OwnMutation);

impl GraphqlFeature for OwnGraphql {
    type Query = OwnFeatureQuery;
    type Mutation = OwnFeatureMutation;

    fn names() -> Vec<&'static str> {
        vec!["own"]
    }

    fn register_types(registry: &mut Registry) {
        register_node::<TeamNode>(registry);
        register_node::<CodeownersIngestedFileNode>(registry);
    }

    fn node_loaders(registry: &mut NodeRegistry<Node>) {
        registry.add(node_loaders::<Node>());
        registry.add([NodeLoader {
            kinds: &[CODEOWNERS_FILE_KIND],
            load: load_codeowners_file,
        }]);
    }
}

/// Loads an ingested CODEOWNERS file (site admins only).
fn load_codeowners_file<'a>(
    ctx: &'a Context<'_>,
    _kind: &'a str,
    id: &'a str,
) -> NodeFuture<'a, Node> {
    Box::pin(async move {
        let Some(key) = unmarshal_int_id(CODEOWNERS_FILE_KIND, id) else {
            return Ok(None);
        };
        Ok(ownership::load_node(ctx, key).await?.map(Node::new))
    })
}

impl From<TeamNode> for Node {
    fn from(node: TeamNode) -> Self {
        Node::new(node)
    }
}
