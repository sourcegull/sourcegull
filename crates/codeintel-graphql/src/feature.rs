//! Code intelligence in the schema.

use async_graphql::MergedObject;
use async_graphql::registry::Registry;
use sourcegull_graphql_core::feature::GraphqlFeature;
use sourcegull_graphql_core::node::{Node, NodeRegistry, register_node};

use crate::autoindex::{AutoIndexMutation, AutoIndexQuery};
use crate::codeintel_policies::{PoliciesMutation, PoliciesQuery, PolicyNode};
use crate::codenav::CodeNavQuery;
use crate::precise_index_coverage::PreciseIndexCoverageQuery;
use crate::precise_indexes::{PreciseIndexNode, PreciseIndexesMutation, PreciseIndexesQuery};
use crate::ranking::{RankingMutation, RankingQuery};
use crate::{CodeIntelNode, node_loaders};

/// The GraphQL feature of code intelligence: code navigation, precise indexes, configuration
/// policies, ranking and auto-indexing.
pub struct CodeIntelGraphql;

/// The root query fields of code intelligence.
#[derive(MergedObject, Default)]
pub struct CodeIntelFeatureQuery(
    CodeNavQuery,
    PreciseIndexesQuery,
    PreciseIndexCoverageQuery,
    PoliciesQuery,
    RankingQuery,
    AutoIndexQuery,
);

/// The root mutation fields of code intelligence.
#[derive(MergedObject, Default)]
pub struct CodeIntelFeatureMutation(
    PreciseIndexesMutation,
    PoliciesMutation,
    RankingMutation,
    AutoIndexMutation,
);

impl GraphqlFeature for CodeIntelGraphql {
    type Query = CodeIntelFeatureQuery;
    type Mutation = CodeIntelFeatureMutation;

    fn names() -> Vec<&'static str> {
        vec!["codeintel"]
    }

    fn register_types(registry: &mut Registry) {
        register_node::<PreciseIndexNode>(registry);
        register_node::<PolicyNode>(registry);
    }

    fn node_loaders(registry: &mut NodeRegistry<Node>) {
        registry.add(node_loaders::<Node>());
    }
}

impl From<CodeIntelNode> for Node {
    fn from(node: CodeIntelNode) -> Self {
        match node {
            CodeIntelNode::PreciseIndex(n) => Node::new(n),
            CodeIntelNode::Policy(p) => Node::new(p),
        }
    }
}
