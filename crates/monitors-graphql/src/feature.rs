//! Watches in the schema.

use async_graphql::registry::Registry;
use sourcegull_graphql_core::feature::GraphqlFeature;
use sourcegull_graphql_core::node::{Node, NodeRegistry, register_node};

use crate::{CodeMonitorsMutation, CodeMonitorsQuery, MonitorNode, node_loaders};

/// The GraphQL feature of watches.
pub struct MonitorsGraphql;

impl GraphqlFeature for MonitorsGraphql {
    type Query = CodeMonitorsQuery;
    type Mutation = CodeMonitorsMutation;

    fn names() -> Vec<&'static str> {
        vec!["monitors"]
    }

    fn register_types(registry: &mut Registry) {
        register_node::<MonitorNode>(registry);
    }

    fn node_loaders(registry: &mut NodeRegistry<Node>) {
        registry.add(node_loaders::<Node>());
    }
}

impl From<MonitorNode> for Node {
    fn from(node: MonitorNode) -> Self {
        Node::new(node)
    }
}
