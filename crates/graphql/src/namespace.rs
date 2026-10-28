//! Namespaces: the users and organizations that own search contexts and
//! saved searches.

use std::borrow::Cow;

use async_graphql::parser::types::Field;
use async_graphql::registry::Registry;
use async_graphql::{
    Context, ContextSelectionSet, ID, Interface, OutputType, Positioned, Result, ServerResult,
    Value,
};
use sourcegull_db::Namespace;
use sourcegull_executors_graphql::executor_secrets::ExecutorSecretConnection;
use sourcegull_graphql_core::shared::NamespaceValue;
use sourcegull_types::{OrgId, UserId};

use crate::context::{db_error, services};
use crate::orgs::OrgNode;
use crate::users::UserNode;

/// A user or an organization.
#[derive(Interface)]
#[graphql(
    name = "Namespace",
    field(name = "id", ty = "ID", desc = "The opaque node ID."),
    field(name = "url", ty = "String", desc = "The namespace's page.")
)]
pub enum NamespaceNode {
    User(UserNode),
    Org(OrgNode),
}

pub use sourcegull_graphql_core::ids::parse_namespace_id;

impl From<NamespaceValue> for NamespaceNode {
    fn from(ns: NamespaceValue) -> Self {
        match ns {
            NamespaceValue::User(u) => NamespaceNode::User(UserNode(u)),
            NamespaceValue::Org(o) => NamespaceNode::Org(OrgNode(o)),
        }
    }
}

/// Loads a live namespace, or `None`.
pub async fn load_namespace(ctx: &Context<'_>, ns: Namespace) -> Result<Option<NamespaceNode>> {
    Ok(sourcegull_graphql_core::shared::load_namespace(ctx, ns)
        .await?
        .map(NamespaceNode::from))
}

/// Whether `user` belongs to the live org `org`.
pub async fn is_member(ctx: &Context<'_>, org: OrgId, user: UserId) -> Result<bool> {
    services(ctx)?
        .db
        .orgs()
        .is_member(org, user)
        .await
        .map_err(db_error)
}

/// The `ExecutorSecretConnection` of `User.executorSecrets` and `Org.executorSecrets`.
/// Registering it registers the `Namespace` interface first (`ExecutorSecret.namespace`
/// returns a reference to it), while `User` or `Org` is being registered, so they list
/// `Namespace` among their interfaces in the order this field reaches it.
pub struct NamespaceExecutorSecrets(pub ExecutorSecretConnection);

impl OutputType for NamespaceExecutorSecrets {
    fn type_name() -> Cow<'static, str> {
        <ExecutorSecretConnection as OutputType>::type_name()
    }

    fn create_type_info(registry: &mut Registry) -> String {
        <NamespaceNode as OutputType>::create_type_info(registry);
        <ExecutorSecretConnection as OutputType>::create_type_info(registry)
    }

    async fn resolve(
        &self,
        ctx: &ContextSelectionSet<'_>,
        field: &Positioned<Field>,
    ) -> ServerResult<Value> {
        OutputType::resolve(&self.0, ctx, field).await
    }
}
