//! The server's own usage events (searches, code navigation, creations; names in
//! [`sourcegull_analytics::names`]) for the admin analytics.

use async_graphql::Context;
use serde_json::Value;
use sourcegull_db::NewUsageEvent;

use crate::context::{services, viewer};

/// The signed-out visitor's id from the `sourcegullAnonymousUid` cookie (request data).
#[derive(Debug, Clone, Default)]
pub struct AnonymousUid(pub Option<String>);

/// Records a server event for the viewer in the background (never fails, never delays the
/// request). Signed-out viewers count only with `auth.public`, by their cookie id.
pub fn log_backend(ctx: &Context<'_>, name: impl Into<String>, argument: Value) {
    let Ok(svc) = services(ctx) else { return };
    let user_id = viewer(ctx).map(|a| a.user_id);
    if user_id.is_none() && !svc.config.load().value.auth_public {
        return;
    }
    let anonymous_user_id = if user_id.is_none() {
        ctx.data_opt::<AnonymousUid>()
            .and_then(|a| a.0.clone())
            .unwrap_or_default()
    } else {
        String::new()
    };
    let event = NewUsageEvent {
        name: name.into(),
        url: String::new(),
        user_id,
        anonymous_user_id,
        source: sourcegull_analytics::names::BACKEND.to_owned(),
        argument,
    };
    let db = svc.db.clone();
    tokio::spawn(async move {
        if let Err(e) = db.event_logs().insert(std::slice::from_ref(&event)).await {
            tracing::warn!(error = %e, event = %event.name, "recording usage event");
        }
    });
}
