//! Outbound webhooks in the schema.

use async_graphql::registry::Registry;
use sourcegull_graphql_core::feature::GraphqlFeature;
use sourcegull_graphql_core::node::{Node, NodeRegistry, register_node};

use crate::{
    OutboundWebhookLogNode, OutboundWebhookNode, OutboundWebhooksMutation, OutboundWebhooksNode,
    OutboundWebhooksQuery, node_loaders,
};

/// The GraphQL feature of outbound webhooks.
pub struct OutboundWebhooksGraphql;

impl GraphqlFeature for OutboundWebhooksGraphql {
    type Query = OutboundWebhooksQuery;
    type Mutation = OutboundWebhooksMutation;

    fn names() -> Vec<&'static str> {
        vec!["outbound-webhooks"]
    }

    fn register_types(registry: &mut Registry) {
        register_node::<OutboundWebhookNode>(registry);
        register_node::<OutboundWebhookLogNode>(registry);
    }

    fn node_loaders(registry: &mut NodeRegistry<Node>) {
        registry.add(node_loaders::<Node>());
    }
}

impl From<OutboundWebhooksNode> for Node {
    fn from(node: OutboundWebhooksNode) -> Self {
        match node {
            OutboundWebhooksNode::Webhook(w) => Node::new(w),
            OutboundWebhooksNode::Log(l) => Node::new(l),
        }
    }
}
