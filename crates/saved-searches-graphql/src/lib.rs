//! The GraphQL API of saved searches and search contexts, merged into the schema by
//! `sourcegull-graphql`:
//!
//! - [`saved_searches`]: the `SavedSearch` type, `savedSearches` and its mutations;
//! - [`search_contexts`]: the `SearchContext` type, the context queries and mutations.

use async_graphql::{Context, Result};
use sourcegull_db::Namespace;
use sourcegull_graphql_core::node::{NodeFuture, NodeLoader};
use sourcegull_graphql_core::shared::{NamespaceRef, load_namespace as load_namespace_value};
use sourcegull_services::relay::unmarshal_int_id;

// `crate::context` and `crate::pagination`, as in the schema assembly.
use sourcegull_graphql_core::{context, pagination};

mod feature;
pub mod saved_searches;
pub mod search_contexts;

pub use feature::{SavedSearchesFeatureMutation, SavedSearchesFeatureQuery, SavedSearchesGraphql};
pub use saved_searches::{
    SAVED_SEARCH_KIND, SavedSearchNode, SavedSearchesMutation, SavedSearchesQuery,
};
pub use search_contexts::{
    SEARCH_CONTEXT_KIND, SearchContextNode, SearchContextsMutation, SearchContextsQuery,
};

/// The node types of this crate.
pub enum SavedSearchesNode {
    /// A search context.
    SearchContext(SearchContextNode),
    /// A saved search.
    SavedSearch(SavedSearchNode),
}

/// The `node(id:)` loaders of search contexts and saved searches.
pub fn node_loaders<N: From<SavedSearchesNode> + Send + 'static>() -> [NodeLoader<N>; 2] {
    fn load_context<'a, N: From<SavedSearchesNode> + Send + 'static>(
        ctx: &'a Context<'_>,
        _kind: &'a str,
        id: &'a str,
    ) -> NodeFuture<'a, N> {
        Box::pin(async move {
            let Some(key) = unmarshal_int_id(SEARCH_CONTEXT_KIND, id) else {
                return Ok(None);
            };
            Ok(search_contexts::load_node(ctx, key)
                .await?
                .map(|c| N::from(SavedSearchesNode::SearchContext(c))))
        })
    }
    fn load_saved_search<'a, N: From<SavedSearchesNode> + Send + 'static>(
        ctx: &'a Context<'_>,
        _kind: &'a str,
        id: &'a str,
    ) -> NodeFuture<'a, N> {
        Box::pin(async move {
            let Some(key) = unmarshal_int_id(SAVED_SEARCH_KIND, id) else {
                return Ok(None);
            };
            Ok(saved_searches::load_node(ctx, key)
                .await?
                .map(|s| N::from(SavedSearchesNode::SavedSearch(s))))
        })
    }
    [
        NodeLoader {
            kinds: &[SEARCH_CONTEXT_KIND],
            load: load_context::<N>,
        },
        NodeLoader {
            kinds: &[SAVED_SEARCH_KIND],
            load: load_saved_search::<N>,
        },
    ]
}

/// Loads a live namespace (a user, or an organization that is not deleted), or `None`.
async fn load_namespace(ctx: &Context<'_>, ns: Namespace) -> Result<Option<NamespaceRef>> {
    Ok(load_namespace_value(ctx, ns).await?.map(NamespaceRef))
}
