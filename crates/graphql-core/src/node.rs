//! The `Node` interface and the `node(id:)` loader registry.
//!
//! [`Node`] holds a value of any object type that implements the interface. The implementations
//! are not listed in one place: each GraphQL feature registers its node types with
//! [`register_node`] and its loaders, by node kind, in the schema's [`NodeRegistry`] (see
//! [`crate::feature::GraphqlFeature`]). A domain crate exports `node_loaders::<N>()`, generic
//! over the node type `N` (`N: From<DomainNode>`).

use std::borrow::Cow;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

use async_graphql::indexmap::{IndexMap, IndexSet};
use async_graphql::parser::types::Field;
use async_graphql::registry::{Deprecation, MetaField, MetaType, MetaTypeId, Registry};
use async_graphql::{
    Context, ContextSelectionSet, ID, ObjectType, OutputType, Positioned, Result, ServerResult,
    Value,
};

use crate::shared::ResolveFuture;

/// The interface's GraphQL name.
const NODE: &str = "Node";

/// An object with an opaque, globally unique ID: a value of an object type registered with
/// [`register_node`].
pub struct Node(Box<dyn NodeObject>);

impl Node {
    /// `object` as a `Node`.
    pub fn new<T: ObjectType + 'static>(object: T) -> Self {
        Self(Box::new(object))
    }
}

/// The object behind a [`Node`].
trait NodeObject: Send + Sync {
    fn type_name(&self) -> Cow<'static, str>;

    fn resolve<'a, 'c>(
        &'a self,
        ctx: &'a ContextSelectionSet<'c>,
        field: &'a Positioned<Field>,
    ) -> ResolveFuture<'a>;
}

impl<T: ObjectType + 'static> NodeObject for T {
    fn type_name(&self) -> Cow<'static, str> {
        T::type_name()
    }

    fn resolve<'a, 'c>(
        &'a self,
        ctx: &'a ContextSelectionSet<'c>,
        field: &'a Positioned<Field>,
    ) -> ResolveFuture<'a> {
        Box::pin(OutputType::resolve(self, ctx, field))
    }
}

impl OutputType for Node {
    fn type_name() -> Cow<'static, str> {
        Cow::Borrowed(NODE)
    }

    fn introspection_type_name(&self) -> Cow<'static, str> {
        self.0.type_name()
    }

    fn create_type_info(registry: &mut Registry) -> String {
        registry.create_output_type::<Self, _>(MetaTypeId::Interface, |registry| {
            let mut fields = IndexMap::new();
            fields.insert(
                "id".to_owned(),
                MetaField {
                    name: "id".to_owned(),
                    description: Some("The opaque node ID.".to_owned()),
                    args: IndexMap::new(),
                    ty: <ID as OutputType>::create_type_info(registry),
                    deprecation: Deprecation::NoDeprecated,
                    cache_control: Default::default(),
                    external: false,
                    provides: None,
                    requires: None,
                    shareable: false,
                    inaccessible: false,
                    tags: Vec::new(),
                    override_from: None,
                    visible: None,
                    compute_complexity: None,
                    directive_invocations: Vec::new(),
                },
            );
            MetaType::Interface {
                name: NODE.to_owned(),
                description: Some("An object with an opaque, globally unique ID.".to_owned()),
                fields,
                possible_types: IndexSet::new(),
                extends: false,
                keys: None,
                visible: None,
                inaccessible: false,
                tags: Vec::new(),
                rust_typename: Some(std::any::type_name::<Self>()),
                directive_invocations: Vec::new(),
            }
        })
    }

    async fn resolve(
        &self,
        ctx: &ContextSelectionSet<'_>,
        field: &Positioned<Field>,
    ) -> ServerResult<Value> {
        self.0.resolve(ctx, field).await
    }
}

/// Registers the object type `T` as an implementation of `Node`.
pub fn register_node<T: ObjectType>(registry: &mut Registry) {
    Node::create_type_info(registry);
    T::create_type_info(registry);
    let name = T::type_name();
    registry.add_implements(&name, NODE);
    if let Some(MetaType::Interface { possible_types, .. }) = registry.types.get_mut(NODE) {
        possible_types.insert(name.into_owned());
    }
}

/// The future a node loader returns.
pub type NodeFuture<'a, N> = Pin<Box<dyn Future<Output = Result<Option<N>>> + Send + 'a>>;

/// Loads the node with ID `id` of kind `kind` (`unmarshal_id(id)`'s kind) if the viewer may
/// see it; `Ok(None)` for unknown, malformed or hidden IDs.
pub type LoadNode<N> = for<'a, 'c> fn(&'a Context<'c>, &'a str, &'a str) -> NodeFuture<'a, N>;

/// The loader of one or more node kinds.
pub struct NodeLoader<N> {
    /// The node kinds `load` handles.
    pub kinds: &'static [&'static str],
    /// The loader.
    pub load: LoadNode<N>,
}

/// Node loaders by kind.
pub struct NodeRegistry<N> {
    by_kind: HashMap<&'static str, LoadNode<N>>,
}

impl<N> Default for NodeRegistry<N> {
    fn default() -> Self {
        Self {
            by_kind: HashMap::new(),
        }
    }
}

impl<N> NodeRegistry<N> {
    /// Adds `loaders`.
    ///
    /// # Panics
    ///
    /// If a kind already has a loader.
    pub fn add(&mut self, loaders: impl IntoIterator<Item = NodeLoader<N>>) -> &mut Self {
        for loader in loaders {
            for kind in loader.kinds {
                let previous = self.by_kind.insert(kind, loader.load);
                assert!(previous.is_none(), "node kind {kind:?} registered twice");
            }
        }
        self
    }

    /// The loader of `kind`.
    pub fn get(&self, kind: &str) -> Option<LoadNode<N>> {
        self.by_kind.get(kind).copied()
    }

    /// The node kinds that have a loader, sorted.
    pub fn kinds(&self) -> Vec<&'static str> {
        let mut kinds: Vec<_> = self.by_kind.keys().copied().collect();
        kinds.sort_unstable();
        kinds
    }
}
