//! Saved searches and search contexts in the schema.

use async_graphql::MergedObject;
use async_graphql::registry::Registry;
use sourcegull_graphql_core::feature::GraphqlFeature;
use sourcegull_graphql_core::node::{Node, NodeRegistry, register_node};

use crate::{
    SavedSearchNode, SavedSearchesMutation, SavedSearchesNode, SavedSearchesQuery,
    SearchContextNode, SearchContextsMutation, SearchContextsQuery, node_loaders,
};

/// The GraphQL feature of saved searches and search contexts.
pub struct SavedSearchesGraphql;

/// The root query fields of saved searches and search contexts.
#[derive(MergedObject, Default)]
pub struct SavedSearchesFeatureQuery(SearchContextsQuery, SavedSearchesQuery);

/// The root mutation fields of saved searches and search contexts.
#[derive(MergedObject, Default)]
pub struct SavedSearchesFeatureMutation(SearchContextsMutation, SavedSearchesMutation);

impl GraphqlFeature for SavedSearchesGraphql {
    type Query = SavedSearchesFeatureQuery;
    type Mutation = SavedSearchesFeatureMutation;

    fn names() -> Vec<&'static str> {
        vec!["saved-searches"]
    }

    fn register_types(registry: &mut Registry) {
        register_node::<SearchContextNode>(registry);
        register_node::<SavedSearchNode>(registry);
    }

    fn node_loaders(registry: &mut NodeRegistry<Node>) {
        registry.add(node_loaders::<Node>());
    }
}

impl From<SavedSearchesNode> for Node {
    fn from(node: SavedSearchesNode) -> Self {
        match node {
            SavedSearchesNode::SearchContext(c) => Node::new(c),
            SavedSearchesNode::SavedSearch(s) => Node::new(s),
        }
    }
}
