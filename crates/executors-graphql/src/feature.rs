//! Executors and executor secrets in the schema.

use async_graphql::MergedObject;
use async_graphql::registry::Registry;
use sourcegull_graphql_core::feature::GraphqlFeature;
use sourcegull_graphql_core::node::{Node, NodeRegistry, register_node};

use crate::executor_secrets::{
    ExecutorSecretAccessLogNode, ExecutorSecretNode, ExecutorSecretsMutation, ExecutorSecretsQuery,
    ExecutorsNode, node_loaders,
};
use crate::executors::ExecutorsQuery;

/// The GraphQL feature of executors and executor secrets.
pub struct ExecutorsGraphql;

/// The root query fields of executors and executor secrets.
#[derive(MergedObject, Default)]
pub struct ExecutorsFeatureQuery(ExecutorsQuery, ExecutorSecretsQuery);

impl GraphqlFeature for ExecutorsGraphql {
    type Query = ExecutorsFeatureQuery;
    type Mutation = ExecutorSecretsMutation;

    fn names() -> Vec<&'static str> {
        vec!["executors"]
    }

    fn register_types(registry: &mut Registry) {
        register_node::<ExecutorSecretNode>(registry);
        register_node::<ExecutorSecretAccessLogNode>(registry);
    }

    fn node_loaders(registry: &mut NodeRegistry<Node>) {
        registry.add(node_loaders::<Node>());
    }
}

impl From<ExecutorsNode> for Node {
    fn from(node: ExecutorsNode) -> Self {
        match node {
            ExecutorsNode::Secret(s) => Node::new(s),
            ExecutorsNode::AccessLog(l) => Node::new(l),
        }
    }
}
