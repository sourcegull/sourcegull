//! Search jobs in the schema.

use async_graphql::registry::Registry;
use sourcegull_graphql_core::feature::GraphqlFeature;
use sourcegull_graphql_core::node::{Node, NodeRegistry, register_node};

use crate::{SearchJobNode, SearchJobsMutation, SearchJobsQuery, node_loaders};

/// The GraphQL feature of search jobs.
pub struct SearchJobsGraphql;

impl GraphqlFeature for SearchJobsGraphql {
    type Query = SearchJobsQuery;
    type Mutation = SearchJobsMutation;

    fn names() -> Vec<&'static str> {
        vec!["searchjobs"]
    }

    fn register_types(registry: &mut Registry) {
        register_node::<SearchJobNode>(registry);
    }

    fn node_loaders(registry: &mut NodeRegistry<Node>) {
        registry.add(node_loaders::<Node>());
    }
}

impl From<SearchJobNode> for Node {
    fn from(node: SearchJobNode) -> Self {
        Node::new(node)
    }
}
