//! The GraphQL API at `/.api/graphql`.
//!
//! The schema uses opaque `base64("User:1")` IDs, `EmptyResponse { alwaysNil }`, `site {
//! configuration { id effectiveContents validationMessages } }`, …, and is split per domain module
//! so it can grow: [`users`], [`site`], [`tokens`], [`orgs`], [`external_services`] (code host
//! connections), [`repos`], [`git`] (code browsing: commits, trees, blobs, highlighting, blame,
//! refs, comparisons), [`batches`] (Batch Changes), plus the [`node`] interface. Each module
//! contributes a query and/or mutation object that is merged into [`QueryRoot`] / [`MutationRoot`].
//!
//! Features add their root fields, node types and node loaders through a
//! [`GraphqlFeature`](sourcegull_graphql_core::feature::GraphqlFeature): the roots are generic over
//! the server's feature list `F` (a tuple of features, `sourcegull::features::Graphql`), whose
//! fields follow the core ones. [`build_schema`], [`sdl`] and [`routes`] take that list.
//!
//! Errors carry a stable `extensions.code` (see [`codes`]). Queries are limited to depth
//! [`MAX_DEPTH`] and complexity [`MAX_COMPLEXITY`]; introspection is on.
//!
//! [`routes`] serves the schema: `POST /.api/graphql` (any operation; the `?OperationName`
//! query string the UI appends is ignored) and `GET /.api/graphql?query=…` (queries only, so
//! that GET, which the CSRF rule exempts, can never change state).

// The merged query and mutation roots are deep enough to need more than the default 128.
#![recursion_limit = "256"]

mod access_requests;
pub mod account;
pub mod admin_logs;
mod audit;
pub mod batches;
mod compute;
mod event_logs;
pub mod external_services;
mod feature_flags;
mod file2;
pub mod git;
mod namespace;
pub mod node;
pub mod oauth_grants;
pub mod oob_migrations;
pub mod orgs;
mod permissions;
mod perms_sync;
mod rbac;
pub mod repos;
mod repos_admin;
pub mod search;
pub mod settings;
mod shared;
pub mod site;
mod status_messages;
mod temporary_settings;
pub mod tokens;
pub mod upgrade_readiness;
pub mod users;

use std::sync::Arc;
use std::time::Duration;

use async_graphql::futures_util::future::BoxFuture;
use async_graphql::parser::types::OperationType;
use async_graphql::{EmptySubscription, MergedObject, ObjectType, Schema, ServerError};
use axum::body::Bytes;
use axum::extract::Extension;
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use sourcegull_authn::Actor;
use sourcegull_http_extract::{MaybeActor, RequestAuth};
use sourcegull_services::AppState;

use sourcegull_graphql_core::context;
use sourcegull_graphql_core::declared;
use sourcegull_graphql_core::feature::{FeatureTypes, GraphqlFeature};
pub use sourcegull_graphql_core::pagination;
pub use sourcegull_graphql_core::{EmptyResponse, Services, Viewer, codes};

/// Maximum query depth.
pub const MAX_DEPTH: usize = 15;
/// Maximum query complexity (roughly, the number of fields resolved).
pub const MAX_COMPLEXITY: usize = 2000;
/// Requests running longer than this are aborted.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// The root query type.
// The core fields, then those of the features `F`.
#[derive(MergedObject, Default)]
pub struct QueryRoot<F: ObjectType>(
    users::UsersQuery,
    access_requests::AccessRequestsQuery,
    site::SiteQuery,
    orgs::OrgsQuery,
    node::NodeQuery,
    external_services::ExternalServicesQuery,
    repos::ReposQuery,
    repos_admin::ReposAdminQuery,
    status_messages::StatusMessagesQuery,
    search::SearchQuery,
    permissions::PermissionsQuery,
    rbac::RbacQuery,
    perms_sync::PermsSyncQuery,
    compute::ComputeQuery,
    oob_migrations::OobMigrationsQuery,
    settings::SettingsQuery,
    admin_logs::AdminLogsQuery,
    feature_flags::FeatureFlagsQuery,
    temporary_settings::TemporarySettingsQuery,
    oauth_grants::OAuthGrantsQuery,
    F,
);

/// The root mutation type.
// The core fields, then those of the features `F`.
#[derive(MergedObject, Default)]
pub struct MutationRoot<F: ObjectType>(
    users::UsersMutation,
    account::AccountMutation,
    access_requests::AccessRequestsMutation,
    site::SiteMutation,
    tokens::TokensMutation,
    orgs::OrgsMutation,
    external_services::ExternalServicesMutation,
    repos::ReposMutation,
    repos_admin::ReposAdminMutation,
    permissions::PermissionsMutation,
    rbac::RbacMutation,
    perms_sync::PermsSyncMutation,
    event_logs::EventLogsMutation,
    oob_migrations::OobMigrationsMutation,
    settings::SettingsMutationRoot,
    admin_logs::AdminLogsMutation,
    feature_flags::FeatureFlagsMutation,
    temporary_settings::TemporarySettingsMutation,
    oauth_grants::OAuthGrantsMutation,
    F,
);

/// The schema builder of the features `F`.
type Builder<F> = async_graphql::SchemaBuilder<
    QueryRoot<<F as GraphqlFeature>::Query>,
    MutationRoot<<F as GraphqlFeature>::Mutation>,
    EmptySubscription,
>;

/// The executable schema, whatever its feature list.
#[derive(Clone)]
pub struct SourcegullSchema(Arc<dyn ExecuteRequest>);

/// A schema's `execute`.
trait ExecuteRequest: Send + Sync {
    fn execute(&self, request: async_graphql::Request) -> BoxFuture<'_, async_graphql::Response>;
}

impl<Q, M> ExecuteRequest for Schema<QueryRoot<Q>, MutationRoot<M>, EmptySubscription>
where
    Q: ObjectType + 'static,
    M: ObjectType + 'static,
{
    fn execute(&self, request: async_graphql::Request) -> BoxFuture<'_, async_graphql::Response> {
        Box::pin(Schema::execute(self, request))
    }
}

/// The schema builder with every type registered, including the shared object types that
/// domain crates return by reference (see [`shared`]) and the types that implement `Node`.
fn schema_builder<F: GraphqlFeature>() -> Builder<F> {
    shared::register::<F>(
        Schema::build(
            QueryRoot::default(),
            MutationRoot::default(),
            EmptySubscription,
        )
        .register_output_type::<git::Ownable>(),
    )
    .register_output_type::<FeatureTypes<node::CoreNodes>>()
    .register_output_type::<FeatureTypes<F>>()
}

/// Builds the schema of the features `F` with `services` as shared data.
pub fn build_schema<F: GraphqlFeature>(services: Services) -> SourcegullSchema {
    SourcegullSchema(Arc::new(
        schema_builder::<F>()
            .data(services)
            .data(node::node_loaders::<F>())
            .limit_depth(MAX_DEPTH)
            .limit_complexity(MAX_COMPLEXITY)
            .finish(),
    ))
}

/// The schema of the features `F` in SDL (for documentation and contract tests).
pub fn sdl<F: GraphqlFeature>() -> String {
    schema_builder::<F>().finish().sdl()
}

/// Executes `request` as `actor`.
pub async fn execute(
    schema: &SourcegullSchema,
    request: async_graphql::Request,
    actor: Option<Actor>,
) -> async_graphql::Response {
    let request = request
        .data(Viewer(actor))
        .data(git::RequestCache::default());
    match tokio::time::timeout(REQUEST_TIMEOUT, schema.0.execute(request)).await {
        Ok(resp) => resp,
        Err(_) => {
            let mut err = ServerError::new("The request timed out.", None);
            err.extensions = Some({
                let mut ext = async_graphql::ErrorExtensionValues::default();
                ext.set("code", "TIMEOUT");
                ext
            });
            async_graphql::Response::from_errors(vec![err])
        }
    }
}

/// The `/.api/graphql` routes for `state`, with the features `F`. Merge into
/// `sourcegull_http_api::public_routes` before `sourcegull_http_api::finish`.
pub fn routes<F: GraphqlFeature>(state: &AppState) -> Router<AppState> {
    let services = Services::from_state(state);
    let schema = build_schema::<F>(services.clone());
    Router::new()
        .route("/.api/graphql", get(graphql_get).post(graphql_post))
        .layer(Extension(schema))
        .layer(Extension(services))
}

/// Executes `request` and logs it in the slow request log when it took longer than
/// `observability.logSlowGraphQLRequests`.
async fn execute_logged(
    schema: &SourcegullSchema,
    services: &Services,
    headers: &HeaderMap,
    request: async_graphql::Request,
    actor: Option<Actor>,
) -> async_graphql::Response {
    let threshold = services
        .config
        .load()
        .value
        .observability_log_slow_graphql_requests;
    let Some(threshold) = threshold else {
        return execute(schema, request, actor).await;
    };
    let name = request
        .operation_name
        .clone()
        .unwrap_or_else(|| "unnamed".to_owned());
    let query = request.query.clone();
    let variables = admin_logs::slow_request_variables(
        &request.query,
        request.operation_name.as_deref(),
        &request.variables,
    );
    let user_id = actor.as_ref().map(|a| a.user_id.get());
    let source = if headers
        .get(sourcegull_http_extract::REQUESTED_WITH_HEADER)
        .is_some()
    {
        "browser"
    } else {
        "other"
    };
    let (started_at, started) = (chrono::Utc::now(), std::time::Instant::now());
    let resp = execute(schema, request, actor).await;
    let elapsed = started.elapsed();
    if sourcegull_services::slow_log::exceeded(Some(threshold), elapsed) {
        sourcegull_services::slow_log::record(
            &services.db,
            sourcegull_db::NewSlowRequest {
                started_at,
                duration_ms: elapsed.as_secs_f64() * 1000.0,
                kind: sourcegull_db::SlowRequestKind::GraphQl,
                user_id,
                name,
                source: source.to_owned(),
                repository: None,
                variables,
                errors: resp.errors.iter().map(|e| e.message.clone()).collect(),
                query,
                filepath: None,
            },
        );
    }
    resp
}

fn json_response(status: StatusCode, resp: &async_graphql::Response) -> Response {
    let mut out = (status, Json(resp)).into_response();
    out.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    out
}

fn request_error(status: StatusCode, message: String) -> Response {
    let resp = async_graphql::Response::from_errors(vec![ServerError::new(message, None)]);
    json_response(status, &resp)
}

/// The signed-out visitor's id from the request's cookie, for usage events.
fn anonymous_uid(headers: &HeaderMap) -> event_logs::AnonymousUid {
    event_logs::AnonymousUid(
        headers
            .get_all(header::COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .find_map(sourcegull_analytics::names::anonymous_uid_from_cookie),
    )
}

async fn graphql_post(
    Extension(schema): Extension<SourcegullSchema>,
    Extension(services): Extension<Services>,
    MaybeActor(actor): MaybeActor,
    auth: Option<Extension<RequestAuth>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let request: async_graphql::Request = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            return request_error(
                StatusCode::BAD_REQUEST,
                format!("Invalid GraphQL request: {e}"),
            );
        }
    };
    let session = auth.and_then(|Extension(a)| a.session);
    let request = request
        .data(anonymous_uid(&headers))
        .data(account::CurrentSession(session));
    json_response(
        StatusCode::OK,
        &execute_logged(&schema, &services, &headers, request, actor).await,
    )
}

async fn graphql_get(
    Extension(schema): Extension<SourcegullSchema>,
    Extension(services): Extension<Services>,
    MaybeActor(actor): MaybeActor,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let mut request = match async_graphql::http::parse_query_string(uri.query().unwrap_or("")) {
        Ok(r) => r,
        Err(e) => {
            return request_error(
                StatusCode::BAD_REQUEST,
                format!("Invalid GraphQL request: {e}"),
            );
        }
    };
    let only_queries = match request.parsed_query() {
        Ok(doc) => doc
            .operations
            .iter()
            .all(|(_, op)| op.node.ty == OperationType::Query),
        Err(e) => {
            return json_response(
                StatusCode::OK,
                &async_graphql::Response::from_errors(vec![e]),
            );
        }
    };
    if !only_queries {
        return request_error(
            StatusCode::METHOD_NOT_ALLOWED,
            "Mutations must be sent with POST.".to_owned(),
        );
    }
    request = request.data(anonymous_uid(&headers));
    json_response(
        StatusCode::OK,
        &execute_logged(&schema, &services, &headers, request, actor).await,
    )
}
