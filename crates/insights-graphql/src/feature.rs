//! Trends in the schema.

use async_graphql::registry::Registry;
use sourcegull_graphql_core::feature::GraphqlFeature;
use sourcegull_graphql_core::node::{Node, NodeRegistry, register_node};

use crate::dashboard::InsightsDashboardNode;
use crate::view::InsightViewNode;
use crate::{InsightsMutation, InsightsNode, InsightsQuery, node_loaders};

/// The GraphQL feature of trends and their dashboards.
pub struct InsightsGraphql;

impl GraphqlFeature for InsightsGraphql {
    type Query = InsightsQuery;
    type Mutation = InsightsMutation;

    fn names() -> Vec<&'static str> {
        vec!["insights"]
    }

    fn register_types(registry: &mut Registry) {
        register_node::<InsightViewNode>(registry);
        register_node::<InsightsDashboardNode>(registry);
    }

    fn node_loaders(registry: &mut NodeRegistry<Node>) {
        registry.add(node_loaders::<Node>());
    }
}

impl From<InsightsNode> for Node {
    fn from(node: InsightsNode) -> Self {
        match node {
            InsightsNode::View(v) => Node::new(v),
            InsightsNode::Dashboard(d) => Node::new(d),
        }
    }
}
