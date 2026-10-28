//! Applying bulk change specs executed with `autoApplyEnabled`: once every
//! workspace execution of such a spec completed, the server applies it as its creator, as if
//! they had run `applyBatchChange` (`sourcegull_services::batches::auto_apply` claims the spec and
//! calls [`GraphqlAutoApplier`]). The apply goes through the GraphQL mutation itself, so it
//! makes exactly the checks of an apply by that user (permissions, readable repositories,
//! the bulk change rules) and records the same security event and webhook.

use std::sync::{Arc, OnceLock};

use async_graphql::{Request, Variables};
use sourcegull_authn::{AuthVia, Authenticator};
use sourcegull_batches::store::BatchSpec;
use sourcegull_graphql_core::feature::GraphqlFeature;
use sourcegull_services::AppState;
use sourcegull_services::batches::AutoApplier;
use sourcegull_types::UserId;

use crate::{Services, SourcegullSchema, build_schema};

const APPLY: &str = "mutation($id: ID!) { applyBatchChange(batchSpec: $id) { id } }";

/// Runs `applyBatchChange` as a spec's creator.
pub struct GraphqlAutoApplier {
    auth: Authenticator,
    schema: OnceLock<SourcegullSchema>,
}

impl std::fmt::Debug for GraphqlAutoApplier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GraphqlAutoApplier").finish_non_exhaustive()
    }
}

impl GraphqlAutoApplier {
    /// Returns `state` with an applier whose schema (of the features `F`) is built from that
    /// same state, so an apply sees every service of the server. (The applier and the state
    /// then refer to each other; both live as long as the process.) Install it before building
    /// the routes.
    pub fn install<F: GraphqlFeature>(state: AppState) -> AppState {
        let applier = Arc::new(Self {
            auth: state.auth.clone(),
            schema: OnceLock::new(),
        });
        let state = state.with_auto_applier(applier.clone());
        let _ = applier
            .schema
            .set(build_schema::<F>(Services::from_state(&state)));
        state
    }
}

#[async_trait::async_trait]
impl AutoApplier for GraphqlAutoApplier {
    async fn apply(&self, spec: &BatchSpec, user: UserId) -> Result<(), String> {
        let schema = self.schema.get().ok_or("the applier is not installed")?;
        let actor = self
            .auth
            .actor_for(user, AuthVia::Session)
            .await
            .map_err(|e| e.to_string())?
            .ok_or("the bulk change spec's creator no longer exists")?;
        let id = super::batch_spec_id(&spec.rand_id);
        let request = Request::new(APPLY).variables(Variables::from_json(
            serde_json::json!({ "id": id.as_str() }),
        ));
        let resp = crate::execute(schema, request, Some(actor)).await;
        if resp.errors.is_empty() {
            Ok(())
        } else {
            Err(resp
                .errors
                .iter()
                .map(|e| e.message.as_str())
                .collect::<Vec<_>>()
                .join("; "))
        }
    }
}
