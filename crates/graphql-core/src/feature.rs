//! A feature's part of the schema: [`GraphqlFeature`].
//!
//! The schema assembly's `QueryRoot` and `MutationRoot` merge the core objects with the query
//! and mutation objects of one `GraphqlFeature`, the server's feature list. A tuple of features
//! is a feature whose objects merge those of its elements in order, so the list is a tuple
//! type (nested tuples for more than 16 elements).

use std::borrow::Cow;
use std::marker::PhantomData;

use async_graphql::parser::types::Field;
use async_graphql::registry::Registry;
use async_graphql::{
    ContextSelectionSet, MergedObject, MergedObjectTail, ObjectType, OutputType, Positioned,
    ServerResult, TypeName, Value,
};

use crate::node::{Node, NodeRegistry};

/// A feature's GraphQL domain: its root fields, the object types that implement `Node`, and
/// the loaders of their node kinds.
pub trait GraphqlFeature: 'static {
    /// The fields the feature adds to `QueryRoot` (an object or a merged object).
    type Query: ObjectType + Default + 'static;
    /// The fields the feature adds to `MutationRoot`; [`MergedObjectTail`] for none.
    type Mutation: ObjectType + Default + 'static;

    /// The feature's name, or the names of a list's features in order.
    fn names() -> Vec<&'static str>;

    /// Registers the feature's `Node` implementations
    /// ([`crate::node::register_node`]) and any other type the root fields do not reach.
    fn register_types(_registry: &mut Registry) {}

    /// Adds the loaders of the feature's node kinds.
    fn node_loaders(_registry: &mut NodeRegistry<Node>) {}
}

/// Registers `F`'s types ([`GraphqlFeature::register_types`]) when a schema builder registers
/// it: `builder.register_output_type::<FeatureTypes<F>>()`. It is not a GraphQL type itself.
pub struct FeatureTypes<F>(PhantomData<fn() -> F>);

impl<F: GraphqlFeature> OutputType for FeatureTypes<F> {
    fn type_name() -> Cow<'static, str> {
        Node::type_name()
    }

    fn create_type_info(registry: &mut Registry) -> String {
        F::register_types(registry);
        Node::create_type_info(registry)
    }

    async fn resolve(
        &self,
        _ctx: &ContextSelectionSet<'_>,
        _field: &Positioned<Field>,
    ) -> ServerResult<Value> {
        Ok(Value::Null)
    }
}

/// The fields of `A`, then those of `B`.
#[derive(MergedObject, Default)]
#[graphql(name_type)]
pub struct Merge<A: ObjectType, B: ObjectType>(A, B);

impl<A: ObjectType, B: ObjectType> TypeName for Merge<A, B> {
    fn type_name() -> Cow<'static, str> {
        Cow::Owned(format!("{}_{}", A::type_name(), B::type_name()))
    }
}

/// No feature.
impl GraphqlFeature for () {
    type Query = MergedObjectTail;
    type Mutation = MergedObjectTail;

    fn names() -> Vec<&'static str> {
        Vec::new()
    }
}

macro_rules! tuple_feature {
    ($a:ident) => {
        impl<$a: GraphqlFeature> GraphqlFeature for ($a,) {
            type Query = <$a as GraphqlFeature>::Query;
            type Mutation = <$a as GraphqlFeature>::Mutation;

            fn names() -> Vec<&'static str> {
                $a::names()
            }

            fn register_types(registry: &mut Registry) {
                $a::register_types(registry);
            }

            fn node_loaders(registry: &mut NodeRegistry<Node>) {
                $a::node_loaders(registry);
            }
        }
    };
    ($a:ident, $($rest:ident),+) => {
        impl<$a: GraphqlFeature, $($rest: GraphqlFeature),+> GraphqlFeature for ($a, $($rest),+) {
            type Query =
                Merge<<$a as GraphqlFeature>::Query, <($($rest,)+) as GraphqlFeature>::Query>;
            type Mutation =
                Merge<<$a as GraphqlFeature>::Mutation, <($($rest,)+) as GraphqlFeature>::Mutation>;

            fn names() -> Vec<&'static str> {
                let mut names = $a::names();
                names.extend(<($($rest,)+) as GraphqlFeature>::names());
                names
            }

            fn register_types(registry: &mut Registry) {
                $a::register_types(registry);
                <($($rest,)+) as GraphqlFeature>::register_types(registry);
            }

            fn node_loaders(registry: &mut NodeRegistry<Node>) {
                $a::node_loaders(registry);
                <($($rest,)+) as GraphqlFeature>::node_loaders(registry);
            }
        }
        tuple_feature!($($rest),+);
    };
}

tuple_feature!(A, B, C, D, E, F, G, H, I, J, K, L, M, N, O, P);
