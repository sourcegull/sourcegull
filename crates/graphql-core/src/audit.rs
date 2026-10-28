//! Security events recorded by GraphQL mutations, honouring
//! `log.securityEventLog.location`.

use serde_json::Value;
use sourcegull_config::SecurityEventLogLocation;
use sourcegull_db::NewSecurityEvent;
use sourcegull_types::UserId;

use crate::context::Services;

/// Records a security event. Never fails: problems are logged.
pub async fn record(services: &Services, name: &str, user_id: Option<UserId>, argument: Value) {
    let location = services.config.load().value.log.security_event_log.location;
    if matches!(
        location,
        SecurityEventLogLocation::Logs | SecurityEventLogLocation::All
    ) {
        tracing::info!(
            target: "security_event",
            event = name,
            user_id = user_id.map(|u| u.get()),
            argument = %argument,
        );
    }
    if matches!(
        location,
        SecurityEventLogLocation::Database | SecurityEventLogLocation::All
    ) {
        let event = NewSecurityEvent {
            name: name.to_owned(),
            user_id,
            source: "BACKEND".to_owned(),
            argument,
            version: env!("CARGO_PKG_VERSION").to_owned(),
            ..Default::default()
        };
        if let Err(e) = services.db.security_events().insert(&event).await {
            tracing::warn!(error = %e, event = name, "recording security event");
        }
    }
}
