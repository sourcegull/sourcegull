//! Bulk changes in the schema.

use async_graphql::registry::Registry;
use sourcegull_graphql_core::feature::GraphqlFeature;
use sourcegull_graphql_core::node::{Node, NodeRegistry, register_node};

use crate::{
    BatchChangeNode, BatchSpecNode, BatchSpecWorkspaceNode, BatchesMutation, BatchesNode,
    BatchesQuery, BulkOperationNode, ChangesetEventNode, ChangesetNode, ChangesetSpecNode,
    CredentialNode, ExternalChangesetNode, HiddenBatchSpecWorkspaceNode, HiddenChangesetSpecNode,
    HiddenExternalChangesetNode, VisibleBatchSpecWorkspaceNode, VisibleChangesetSpecNode,
    node_loaders,
};

/// The GraphQL feature of bulk changes.
pub struct BatchesGraphql;

impl GraphqlFeature for BatchesGraphql {
    type Query = BatchesQuery;
    type Mutation = BatchesMutation;

    fn names() -> Vec<&'static str> {
        vec!["batches"]
    }

    fn register_types(registry: &mut Registry) {
        register_node::<BatchChangeNode>(registry);
        register_node::<BatchSpecNode>(registry);
        register_node::<VisibleChangesetSpecNode>(registry);
        register_node::<HiddenChangesetSpecNode>(registry);
        register_node::<ExternalChangesetNode>(registry);
        register_node::<HiddenExternalChangesetNode>(registry);
        register_node::<ChangesetEventNode>(registry);
        register_node::<VisibleBatchSpecWorkspaceNode>(registry);
        register_node::<HiddenBatchSpecWorkspaceNode>(registry);
        register_node::<CredentialNode>(registry);
        register_node::<BulkOperationNode>(registry);
    }

    fn node_loaders(registry: &mut NodeRegistry<Node>) {
        registry.add(node_loaders::<Node>());
    }
}

impl From<BatchesNode> for Node {
    fn from(node: BatchesNode) -> Self {
        match node {
            BatchesNode::BatchChange(b) => Node::new(b),
            BatchesNode::BatchSpec(s) => Node::new(s),
            BatchesNode::ChangesetSpec(ChangesetSpecNode::VisibleChangesetSpec(v)) => Node::new(v),
            BatchesNode::ChangesetSpec(ChangesetSpecNode::HiddenChangesetSpec(h)) => Node::new(h),
            BatchesNode::Changeset(ChangesetNode::ExternalChangeset(e)) => Node::new(e),
            BatchesNode::Changeset(ChangesetNode::HiddenExternalChangeset(h)) => Node::new(h),
            BatchesNode::Workspace(BatchSpecWorkspaceNode::VisibleBatchSpecWorkspace(v)) => {
                Node::new(v)
            }
            BatchesNode::Workspace(BatchSpecWorkspaceNode::HiddenBatchSpecWorkspace(h)) => {
                Node::new(h)
            }
            BatchesNode::Credential(c) => Node::new(c),
            BatchesNode::BulkOperation(b) => Node::new(b),
        }
    }
}
