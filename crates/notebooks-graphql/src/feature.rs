//! Notebooks in the schema.

use async_graphql::registry::Registry;
use sourcegull_graphql_core::feature::GraphqlFeature;
use sourcegull_graphql_core::node::{Node, NodeRegistry, register_node};

use crate::{NotebookNode, NotebooksMutation, NotebooksQuery, node_loaders};

/// The GraphQL feature of notebooks.
pub struct NotebooksGraphql;

impl GraphqlFeature for NotebooksGraphql {
    type Query = NotebooksQuery;
    type Mutation = NotebooksMutation;

    fn names() -> Vec<&'static str> {
        vec!["notebooks"]
    }

    fn register_types(registry: &mut Registry) {
        register_node::<NotebookNode>(registry);
    }

    fn node_loaders(registry: &mut NodeRegistry<Node>) {
        registry.add(node_loaders::<Node>());
    }
}

impl From<NotebookNode> for Node {
    fn from(node: NotebookNode) -> Self {
        Node::new(node)
    }
}
