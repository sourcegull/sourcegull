//! The features the server runs. The `web` role merges their routes into the public router
//! ([`crate::server::public_router`]) and serves their part of the GraphQL schema ([`Graphql`]);
//! the `worker` role runs their queue workers and singletons ([`crate::worker::run`]). A new
//! feature implements [`Feature`] in its domain crate and is added to [`all`], and implements
//! `sourcegull_graphql_core::feature::GraphqlFeature` in its `-graphql` crate and is added to
//! [`Graphql`].

use axum::Router;
use sourcegull_feature::Feature;
use sourcegull_graphql::{Services, SourcegullSchema};
use sourcegull_services::AppState;

/// Every feature's part of the GraphQL schema, in the order of their root fields (after the
/// core ones).
pub type Graphql = (
    sourcegull_saved_searches_graphql::SavedSearchesGraphql,
    sourcegull_searchjobs_graphql::SearchJobsGraphql,
    sourcegull_codeintel_graphql::CodeIntelGraphql,
    sourcegull_executors_graphql::ExecutorsGraphql,
    sourcegull_batches_graphql::BatchesGraphql,
    sourcegull_outbound_webhooks_graphql::OutboundWebhooksGraphql,
    sourcegull_notebooks_graphql::NotebooksGraphql,
    sourcegull_monitors_graphql::MonitorsGraphql,
    sourcegull_own_graphql::OwnGraphql,
    sourcegull_insights_graphql::InsightsGraphql,
);

/// The GraphQL schema with every feature, with `services` as shared data.
pub fn graphql_schema(services: Services) -> SourcegullSchema {
    sourcegull_graphql::build_schema::<Graphql>(services)
}

/// The `/.api/graphql` routes with every feature.
pub fn graphql_routes(state: &AppState) -> Router<AppState> {
    sourcegull_graphql::routes::<Graphql>(state)
}

/// The GraphQL schema with every feature, in SDL.
pub fn graphql_sdl() -> String {
    sourcegull_graphql::sdl::<Graphql>()
}

/// Installs the bulk change auto-applier with every feature's schema.
pub fn install_auto_applier(state: AppState) -> AppState {
    sourcegull_graphql::batches::auto_apply::GraphqlAutoApplier::install::<Graphql>(state)
}

/// Every feature, in the order their tasks are started.
pub fn all() -> Vec<Box<dyn Feature<AppState>>> {
    vec![
        Box::new(sourcegull_authz::PermsSyncFeature),
        Box::new(sourcegull_codeintel_scip::UploadsFeature),
        Box::new(sourcegull_ranking::RankingFeature),
        Box::new(sourcegull_autoindex::AutoIndexFeature),
        Box::new(sourcegull_codeintel_policies::RetentionFeature),
        Box::new(sourcegull_batches_resolver::ResolverFeature),
        Box::new(sourcegull_searchjobs::SearchJobsFeature),
        Box::new(sourcegull_outbound_webhooks::OutboundWebhooksFeature),
        Box::new(sourcegull_batches_reconciler::ReconcilerFeature),
        Box::new(sourcegull_monitors::WatchesFeature),
        Box::new(sourcegull_insights::InsightsFeature),
        Box::new(sourcegull_own::OwnFeature),
        Box::new(sourcegull_oobmigration::OobMigrationFeature),
        Box::new(sourcegull_analytics::AnalyticsFeature),
        Box::new(sourcegull_repos::ReposFeature),
        Box::new(sourcegull_mcp::McpFeature),
        Box::new(sourcegull_oauth_server::OAuthServerFeature),
    ]
}
