//! Local usage events: `logEvent` and `logEvents`, writing
//! `event_logs`. Nothing leaves the instance. The file page logs `ViewBlob` (argument
//! `{"repoName", "filePath"}`), which the recent views ownership signal counts. A
//! `ViewBlob` for a repository the viewer may not read is dropped, so nobody becomes
//! a "recent viewer" of a file they cannot see.
//!
//! Only the fields Sourcegull stores are read: the event name, the URL, the source, the
//! anonymous user cookie and the (public) argument. Session, cohort and
//! referrer fields are accepted and ignored. Signed-out visitors' events are recorded (with
//! their random id) only when anonymous access is allowed (`auth.public`); otherwise they are
//! dropped.
//!
//! [`log_backend`] records the server's own events (searches, code navigation, creations;
//! names in [`sourcegull_analytics::names`]) for the admin analytics.

use async_graphql::{Context, Enum, InputObject, Object, Result};
use serde_json::Value;

use sourcegull_db::NewUsageEvent;

use crate::EmptyResponse;
use crate::context::{codes, db_error, error, services, viewer};

pub use sourcegull_graphql_core::events::AnonymousUid;

/// Events per `logEvents` call.
const MAX_EVENTS: usize = 100;
/// The file view event.
const VIEW_BLOB: &str = "ViewBlob";
/// Bytes of an event's argument.
const MAX_ARGUMENT_BYTES: usize = 16 * 1024;

/// Where an event comes from.
#[derive(Enum, Copy, Clone, Debug, PartialEq, Eq)]
pub enum EventSource {
    Web,
    Codehostintegration,
    Backend,
    Staticweb,
    Ideextension,
}

impl EventSource {
    fn as_str(self) -> &'static str {
        match self {
            EventSource::Web => "WEB",
            EventSource::Codehostintegration => "CODEHOSTINTEGRATION",
            EventSource::Backend => "BACKEND",
            EventSource::Staticweb => "STATICWEB",
            EventSource::Ideextension => "IDEEXTENSION",
        }
    }
}

/// One event of `logEvents`.
#[derive(InputObject)]
pub struct Event {
    pub event: String,
    #[graphql(name = "userCookieID")]
    pub user_cookie_id: String,
    #[graphql(name = "firstSourceURL")]
    pub first_source_url: Option<String>,
    #[graphql(name = "lastSourceURL")]
    pub last_source_url: Option<String>,
    pub url: String,
    pub source: EventSource,
    #[graphql(name = "cohortID")]
    pub cohort_id: Option<String>,
    pub referrer: Option<String>,
    pub original_referrer: Option<String>,
    pub session_referrer: Option<String>,
    #[graphql(name = "sessionFirstURL")]
    pub session_first_url: Option<String>,
    #[graphql(name = "deviceSessionID")]
    pub device_session_id: Option<String>,
    /// A JSON object as a string.
    pub argument: Option<String>,
    /// A JSON object as a string; merged into `argument`.
    pub public_argument: Option<String>,
    #[graphql(name = "deviceID")]
    pub device_id: Option<String>,
    #[graphql(name = "eventID")]
    pub event_id: Option<i32>,
    #[graphql(name = "insertID")]
    pub insert_id: Option<String>,
}

fn parse_argument(arg: Option<&str>) -> Result<serde_json::Map<String, Value>> {
    let Some(arg) = arg.filter(|a| !a.trim().is_empty()) else {
        return Ok(serde_json::Map::new());
    };
    if arg.len() > MAX_ARGUMENT_BYTES {
        return Err(error(
            codes::INVALID_INPUT,
            "The event argument is too large.",
        ));
    }
    match serde_json::from_str(arg) {
        Ok(Value::Object(m)) => Ok(m),
        _ => Err(error(
            codes::INVALID_INPUT,
            "The event argument must be a JSON object.",
        )),
    }
}

async fn insert(ctx: &Context<'_>, events: &[Event]) -> Result<()> {
    if events.len() > MAX_EVENTS {
        return Err(error(
            codes::INVALID_INPUT,
            format!("At most {MAX_EVENTS} events can be logged at once."),
        ));
    }
    let user_id = viewer(ctx).map(|a| a.user_id);
    if user_id.is_none() && !services(ctx)?.config.load().value.auth_public {
        // Signed-out visitors are counted only where anonymous access is allowed.
        return Ok(());
    }
    let mut access = None;
    let mut rows = Vec::with_capacity(events.len());
    for e in events {
        let name = e.event.trim();
        if name.is_empty() || name.len() > 256 {
            return Err(error(codes::INVALID_INPUT, "Invalid event name."));
        }
        let mut arg = parse_argument(e.argument.as_deref())?;
        arg.extend(parse_argument(e.public_argument.as_deref())?);
        if name == VIEW_BLOB {
            let repo = arg
                .get("repoName")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if access.is_none() {
                access = Some(crate::context::repo_access(ctx).await?);
            }
            let readable = !repo.is_empty()
                && services(ctx)?
                    .db
                    .repos()
                    .get_by_name_authorized(repo, access.as_ref().expect("set above"))
                    .await
                    .map_err(db_error)?
                    .is_some();
            if !readable {
                continue;
            }
        }
        rows.push(NewUsageEvent {
            name: name.to_owned(),
            url: e.url.chars().take(2048).collect(),
            user_id,
            anonymous_user_id: e.user_cookie_id.chars().take(256).collect(),
            source: e.source.as_str().to_owned(),
            argument: Value::Object(arg),
        });
    }
    services(ctx)?
        .db
        .event_logs()
        .insert(&rows)
        .await
        .map_err(db_error)
}

#[derive(Default)]
pub struct EventLogsMutation;

#[Object]
impl EventLogsMutation {
    /// Logs a usage event for the viewer (or an anonymous visitor).
    #[allow(clippy::too_many_arguments)]
    async fn log_event(
        &self,
        ctx: &Context<'_>,
        event: String,
        #[graphql(name = "userCookieID")] user_cookie_id: String,
        #[graphql(name = "firstSourceURL")] first_source_url: Option<String>,
        #[graphql(name = "lastSourceURL")] last_source_url: Option<String>,
        url: String,
        source: EventSource,
        #[graphql(name = "cohortID")] cohort_id: Option<String>,
        referrer: Option<String>,
        argument: Option<String>,
        public_argument: Option<String>,
    ) -> Result<Option<EmptyResponse>> {
        let e = Event {
            event,
            user_cookie_id,
            first_source_url,
            last_source_url,
            url,
            source,
            cohort_id,
            referrer,
            original_referrer: None,
            session_referrer: None,
            session_first_url: None,
            device_session_id: None,
            argument,
            public_argument,
            device_id: None,
            event_id: None,
            insert_id: None,
        };
        insert(ctx, std::slice::from_ref(&e)).await?;
        Ok(None)
    }

    /// Logs several usage events.
    async fn log_events(
        &self,
        ctx: &Context<'_>,
        events: Option<Vec<Event>>,
    ) -> Result<Option<EmptyResponse>> {
        insert(ctx, events.as_deref().unwrap_or_default()).await?;
        Ok(None)
    }
}
