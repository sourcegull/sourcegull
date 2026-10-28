//! Code intelligence over GraphQL:
//!
//! - code navigation: `GitBlob.symbolInfo`, `GitBlob.codeGraphData` and `usagesForSymbol`
//!   ([`codenav`]);
//! - precise indexes: `preciseIndexes`, `indexerKeys`, deleting and reindexing
//!   ([`precise_indexes`]) and their coverage of repositories ([`precise_index_coverage`]);
//! - configuration policies, retention and the git object filter preview
//!   ([`codeintel_policies`]);
//! - auto-indexing: the inference script, inferred jobs and `Repository.indexConfiguration`
//!   ([`autoindex`]);
//! - the ranking job ([`ranking`]).
//!
//! Fields of the shared git and repository types that return these types stay in the schema
//! assembly and call the public functions here.

pub mod autoindex;
pub mod codeintel_policies;
pub mod codenav;
mod feature;
pub mod precise_index_coverage;
pub mod precise_indexes;
pub mod ranking;

pub use feature::{CodeIntelFeatureMutation, CodeIntelFeatureQuery, CodeIntelGraphql};

// `crate::context` and `crate::pagination`, as in the schema assembly.
use sourcegull_graphql_core::{context, pagination};

use async_graphql::{Context, Result};
use sourcegull_graphql_core::node::{NodeFuture, NodeLoader};
use sourcegull_services::relay::unmarshal_id;

use codeintel_policies::{POLICY_KIND, PolicyNode};
use precise_indexes::{PRECISE_INDEX_KIND, PreciseIndexNode};

/// The node types of code intelligence.
// Values are short-lived (one per resolved field), so the size difference does not matter.
#[allow(clippy::large_enum_variant)]
pub enum CodeIntelNode {
    /// A precise code intelligence index.
    PreciseIndex(PreciseIndexNode),
    /// A code intelligence configuration policy.
    Policy(PolicyNode),
}

/// The `node(id:)` loaders of precise indexes and policies, for the schema's node type `N`.
pub fn node_loaders<N: From<CodeIntelNode> + Send + 'static>() -> [NodeLoader<N>; 1] {
    fn load<'a, N: From<CodeIntelNode> + Send + 'static>(
        ctx: &'a Context<'_>,
        kind: &'a str,
        id: &'a str,
    ) -> NodeFuture<'a, N> {
        Box::pin(async move { Ok(load_node(ctx, kind, id).await?.map(N::from)) })
    }
    [NodeLoader {
        kinds: &[PRECISE_INDEX_KIND, POLICY_KIND],
        load: load::<N>,
    }]
}

/// Looks up a precise index or policy node the viewer sees.
pub async fn load_node(ctx: &Context<'_>, kind: &str, id: &str) -> Result<Option<CodeIntelNode>> {
    match kind {
        PRECISE_INDEX_KIND => Ok(match precise_indexes::parse_precise_index_id(id) {
            Some(key) => precise_indexes::load_node(ctx, key)
                .await?
                .map(CodeIntelNode::PreciseIndex),
            None => None,
        }),
        POLICY_KIND => {
            let Some(key) = unmarshal_id(id)
                .and_then(|(_, spec)| spec.parse::<i64>().ok())
                .and_then(|key| i32::try_from(key).ok())
            else {
                return Ok(None);
            };
            Ok(codeintel_policies::load_node(ctx, key)
                .await?
                .map(CodeIntelNode::Policy))
        }
        _ => Ok(None),
    }
}
