//! The `node(id:)` lookup (Relay global object identification) and the core types that
//! implement `Node`.
//!
//! The `Node` interface is [`sourcegull_graphql_core::node::Node`]. Features register their node
//! types and the loaders of their node kinds ([`sourcegull_graphql_core::feature::GraphqlFeature`]);
//! the schema holds the loaders as a [`NodeRegistry`]. The core kinds are matched in
//! [`NodeQuery::node`].

use async_graphql::registry::Registry;
use async_graphql::{Context, ID, MergedObjectTail, Object, Result};
use sourcegull_graphql_core::feature::GraphqlFeature;
use sourcegull_graphql_core::node::{NodeRegistry, register_node};
use sourcegull_services::relay::{EXTERNAL_SERVICE_KIND, unmarshal_id};
use sourcegull_types::{OrgId, UserId};

pub use sourcegull_graphql_core::node::Node;

use crate::admin_logs::{BackgroundJobNode, OutboundRequestNode, WebhookLogNode};
use crate::context::{db_error, require_viewer, services};
use crate::external_services::ExternalServiceNode;
use crate::oob_migrations::OutOfBandMigrationNode;
use crate::orgs::{ORG_KIND, OrgInvitationNode, OrgNode};
use crate::repos::{REPOSITORY_KIND, RepositoryNode};
use crate::users::{USER_KIND, UserNode};

/// The core types that implement `Node`, registered like a feature's.
pub(crate) struct CoreNodes;

impl GraphqlFeature for CoreNodes {
    type Query = MergedObjectTail;
    type Mutation = MergedObjectTail;

    fn names() -> Vec<&'static str> {
        Vec::new()
    }

    fn register_types(registry: &mut Registry) {
        register_node::<UserNode>(registry);
        register_node::<OrgNode>(registry);
        register_node::<OrgInvitationNode>(registry);
        register_node::<ExternalServiceNode>(registry);
        register_node::<RepositoryNode>(registry);
        register_node::<OutOfBandMigrationNode>(registry);
        register_node::<BackgroundJobNode>(registry);
        register_node::<WebhookLogNode>(registry);
        register_node::<OutboundRequestNode>(registry);
    }
}

/// The node loaders of the features `F`, by kind.
pub(crate) fn node_loaders<F: GraphqlFeature>() -> NodeRegistry<Node> {
    let mut registry = NodeRegistry::default();
    F::node_loaders(&mut registry);
    registry
}

/// The `node` query.
#[derive(Default)]
pub struct NodeQuery;

#[Object]
impl NodeQuery {
    /// Looks up a user, organization, repository, search context, saved search, search job, precise
    /// index, code intelligence configuration policy, executor secret or access log entry,
    /// bulk change, bulk change spec, changeset spec, changeset, workspace, batches credential, notebook,
    /// watch, team, trend, trends dashboard, or
    /// (site admins only) code host connection, outbound webhook, delivery log, ingested
    /// CODEOWNERS file, out-of-band migration, background job, webhook log or outbound request
    /// by its node
    /// ID (signed-in users only). Returns null for unknown or malformed IDs.
    async fn node(&self, ctx: &Context<'_>, id: ID) -> Result<Option<Node>> {
        require_viewer(ctx)?;
        let Some((kind, spec)) = unmarshal_id(&id) else {
            return Ok(None);
        };
        if let Some(load) = ctx.data::<NodeRegistry<Node>>()?.get(&kind) {
            return load(ctx, &kind, &id).await;
        }
        if let Some(node) = crate::admin_logs::load_node(ctx, &kind, &id).await? {
            return Ok(node);
        }
        if kind == crate::oob_migrations::OOB_MIGRATION_KIND {
            return Ok(crate::oob_migrations::load_node(ctx, &id)
                .await?
                .map(Node::new));
        }
        let Ok(key) = spec.parse::<i64>() else {
            return Ok(None);
        };
        let db = &services(ctx)?.db;
        Ok(match kind.as_str() {
            USER_KIND => db
                .users()
                .get_by_id(UserId(key))
                .await
                .map_err(db_error)?
                .map(|u| Node::new(UserNode(u))),
            ORG_KIND => db
                .orgs()
                .get(OrgId(key))
                .await
                .map_err(db_error)?
                .map(|o| Node::new(OrgNode(o))),
            REPOSITORY_KIND => crate::repos::load_node(ctx, key).await?.map(Node::new),
            crate::orgs::ORG_INVITATION_KIND => crate::orgs::load_invitation_node(ctx, key)
                .await?
                .map(Node::new),
            EXTERNAL_SERVICE_KIND => {
                crate::context::require_site_admin(ctx)?;
                crate::external_services::load_node(services(ctx)?, key)
                    .await?
                    .map(Node::new)
            }
            _ => None,
        })
    }
}
