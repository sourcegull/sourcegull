//! Outbound webhooks, site admins only:
//! `outboundWebhooks`, `outboundWebhookEventTypes`, `createOutboundWebhook`,
//! `updateOutboundWebhook`, `deleteOutboundWebhook`, and the `OutboundWebhook` (with its
//! `stats` and `logs`) and `OutboundWebhookLog` nodes, and `channelDeliveryLogs`, the attempts
//! of watch actions that post to a URL.
//!
//! Secrets are write-only. URLs must be `http` or `https`; an IP literal must be public or in
//! `webhooks.outbound.allowedHosts` (host names are checked when a delivery resolves them).
//! Changes are recorded as security events (without the URL, which may carry a token).

use async_graphql::{Context, Enum, ID, InputObject, Object, Result, SimpleObject};
use chrono::{DateTime, Utc};
use serde_json::json;
use sourcegull_db::DbError;
use sourcegull_graphql_core::mask::{keep_masked, mask_url};
use sourcegull_outbound_webhooks::EVENT_TYPES;
use sourcegull_outbound_webhooks::channel::Channel;
use sourcegull_outbound_webhooks::guard::check_webhook_url;
use sourcegull_outbound_webhooks::store::{
    ChannelDelivery, ListOutboundWebhooks, LoggedRequest, LoggedResponse, NewOutboundWebhook,
    OutboundWebhook, OutboundWebhookLog, OutboundWebhookStore, OutboundWebhookUpdate,
    ScopedEventType,
};
use sourcegull_services::relay::{marshal_id, unmarshal_int_id};

mod feature;

pub use feature::OutboundWebhooksGraphql;

// `crate::context` and `crate::pagination`, as in the schema assembly.
use sourcegull_graphql_core::{context, pagination};

use crate::context::{codes, db_error, error, page_size, require_site_admin, services, to_int};
use crate::pagination::{PageInfo, decode_cursor};
use sourcegull_graphql_core::node::{NodeFuture, NodeLoader};
use sourcegull_graphql_core::shared::UserRef;
use sourcegull_graphql_core::{EmptyResponse, audit, declared};

/// Security event names.
mod events {
    /// An outbound webhook was created.
    pub const OUTBOUND_WEBHOOK_CREATED: &str = "OutboundWebhookCreated";
    /// An outbound webhook was updated.
    pub const OUTBOUND_WEBHOOK_UPDATED: &str = "OutboundWebhookUpdated";
    /// An outbound webhook was deleted.
    pub const OUTBOUND_WEBHOOK_DELETED: &str = "OutboundWebhookDeleted";
}

/// Node kind of outbound webhook IDs.
pub const OUTBOUND_WEBHOOK_KIND: &str = "OutboundWebhook";
/// Node kind of delivery log IDs.
pub const OUTBOUND_WEBHOOK_LOG_KIND: &str = "OutboundWebhookLog";
/// Kind of channel delivery log IDs.
pub const CHANNEL_DELIVERY_LOG_KIND: &str = "ChannelDeliveryLog";

const MAX_PAGE: i64 = 1000;

fn invalid(message: impl Into<String>) -> async_graphql::Error {
    error(codes::INVALID_INPUT, message)
}

fn store_error(e: DbError) -> async_graphql::Error {
    match e {
        DbError::Invalid(m) => error(codes::UNAVAILABLE, m),
        e => db_error(e),
    }
}

fn not_found() -> async_graphql::Error {
    error(codes::NOT_FOUND, "Outbound webhook not found.")
}

/// An event type with its description.
#[derive(SimpleObject)]
#[graphql(name = "OutboundWebhookEventType")]
pub struct OutboundWebhookEventType {
    /// The event type, e.g. `changeset:publish`.
    key: String,
    description: String,
}

/// An event type a webhook subscribes to.
#[derive(SimpleObject)]
#[graphql(name = "OutboundWebhookScopedEventType")]
pub struct ScopedEventTypeNode {
    event_type: String,
    /// Only events of this scope; null for all.
    scope: Option<String>,
}

/// An event type to subscribe to.
#[derive(InputObject)]
#[graphql(name = "OutboundWebhookScopedEventTypeInput")]
pub struct ScopedEventTypeInput {
    /// One of `outboundWebhookEventTypes`.
    event_type: String,
    /// Only events of this scope; null or absent for all.
    scope: Option<String>,
}

/// A new webhook.
#[derive(InputObject)]
#[graphql(name = "OutboundWebhookCreateInput")]
pub struct OutboundWebhookCreateInput {
    /// The `http` or `https` URL events are posted to.
    url: String,
    /// The key of the `X-Sourcegull-Webhook-Signature` HMAC-SHA256 of each body.
    secret: String,
    /// At least one.
    event_types: Vec<ScopedEventTypeInput>,
}

/// Changes to a webhook: the URL and event types are replaced, the secret when given.
#[derive(InputObject)]
#[graphql(name = "OutboundWebhookUpdateInput")]
pub struct OutboundWebhookUpdateInput {
    /// Empty or the masked URL keeps the stored one.
    url: String,
    event_types: Vec<ScopedEventTypeInput>,
    /// A new secret (unchanged when absent).
    secret: Option<String>,
}

/// Counts of a webhook's delivery attempts.
#[derive(SimpleObject)]
#[graphql(name = "OutboundWebhookLogStats")]
pub struct OutboundWebhookLogStats {
    total: i32,
    /// Attempts without a 2xx response.
    errored: i32,
}

/// A webhook (without its secret).
pub struct OutboundWebhookNode(pub OutboundWebhook);

#[Object(name = "OutboundWebhook")]
impl OutboundWebhookNode {
    /// The opaque node ID.
    pub async fn id(&self) -> ID {
        ID(marshal_id(OUTBOUND_WEBHOOK_KIND, self.0.id))
    }

    /// The URL events are posted to, masked: the scheme, host and port, then the last
    /// characters of the rest (`https://hooks.example.com/…a9f3`).
    async fn url(&self) -> String {
        mask_url(&self.0.url)
    }

    /// The subscribed event types.
    async fn event_types(&self) -> Vec<ScopedEventTypeNode> {
        self.0
            .event_types
            .iter()
            .map(|t| ScopedEventTypeNode {
                event_type: t.event_type.clone(),
                scope: t.scope.clone(),
            })
            .collect()
    }

    /// The resources file entry that manages the webhook (`outboundWebhooks[<key>]`), or
    /// `outboundWebhooks` when the file manages every webhook; such webhooks are read-only.
    async fn managed_by(&self, ctx: &Context<'_>) -> Result<Option<String>> {
        declared::managed_by(
            services(ctx)?,
            declared::kinds::OUTBOUND_WEBHOOKS,
            &self.0.id.to_string(),
        )
        .await
    }

    /// Counts of the delivery attempts still logged.
    async fn stats(&self, ctx: &Context<'_>) -> Result<OutboundWebhookLogStats> {
        let stats = OutboundWebhookStore::new(&services(ctx)?.db, None)
            .log_stats(self.0.id)
            .await
            .map_err(db_error)?;
        Ok(OutboundWebhookLogStats {
            total: to_int(stats.total),
            errored: to_int(stats.errored),
        })
    }

    /// Delivery attempts, newest first; only failed ones with `onlyErrors`.
    async fn logs(
        &self,
        ctx: &Context<'_>,
        #[graphql(default_with = "Some(50)")] first: Option<i32>,
        after: Option<String>,
        #[graphql(default_with = "Some(false)")] only_errors: Option<bool>,
    ) -> Result<OutboundWebhookLogConnection> {
        let only_errors = only_errors.unwrap_or(false);
        let limit = page_size(first, 50, MAX_PAGE);
        let offset = decode_cursor(after.as_deref())?;
        let services = services(ctx)?;
        let store = OutboundWebhookStore::new(&services.db, services.keyring.clone());
        let mut logs = store
            .logs(self.0.id, only_errors, limit + 1, offset)
            .await
            .map_err(store_error)?;
        let stats = store.log_stats(self.0.id).await.map_err(db_error)?;
        let page_info = PageInfo::new(offset, limit, logs.len());
        logs.truncate(usize::try_from(limit).unwrap_or(0));
        Ok(OutboundWebhookLogConnection {
            nodes: logs.into_iter().map(OutboundWebhookLogNode).collect(),
            total_count: to_int(if only_errors {
                stats.errored
            } else {
                stats.total
            }),
            page_info,
        })
    }

    /// The creator; null once deleted.
    async fn created_by(&self, ctx: &Context<'_>) -> Result<Option<UserRef>> {
        user(ctx, self.0.created_by).await
    }

    async fn created_at(&self) -> DateTime<Utc> {
        self.0.created_at
    }

    /// Who updated the webhook last; null once deleted.
    async fn updated_by(&self, ctx: &Context<'_>) -> Result<Option<UserRef>> {
        user(ctx, self.0.updated_by).await
    }

    async fn updated_at(&self) -> DateTime<Utc> {
        self.0.updated_at
    }
}

async fn user(ctx: &Context<'_>, id: Option<sourcegull_types::UserId>) -> Result<Option<UserRef>> {
    match id {
        Some(id) => Ok(services(ctx)?
            .db
            .users()
            .get_by_id(id)
            .await
            .map_err(db_error)?
            .map(UserRef)),
        None => Ok(None),
    }
}

/// A page of webhooks.
#[derive(SimpleObject)]
#[graphql(name = "OutboundWebhookConnection")]
pub struct OutboundWebhookConnection {
    nodes: Vec<OutboundWebhookNode>,
    total_count: i32,
    page_info: PageInfo,
}

/// A page of delivery attempts.
#[derive(SimpleObject)]
#[graphql(name = "OutboundWebhookLogConnection")]
pub struct OutboundWebhookLogConnection {
    nodes: Vec<OutboundWebhookLogNode>,
    total_count: i32,
    page_info: PageInfo,
}

/// An HTTP header and its values.
#[derive(SimpleObject)]
#[graphql(name = "HTTPHeader")]
pub struct HttpHeader {
    /// The header name.
    pub name: String,
    /// Its values.
    pub values: Vec<String>,
}

fn headers(map: &std::collections::BTreeMap<String, Vec<String>>) -> Vec<HttpHeader> {
    map.iter()
        .map(|(name, values)| HttpHeader {
            name: name.clone(),
            values: values.clone(),
        })
        .collect()
}

/// The request of a delivery attempt.
pub struct WebhookLogRequestNode(pub LoggedRequest);

#[Object(name = "WebhookLogRequest")]
impl WebhookLogRequestNode {
    async fn headers(&self) -> Vec<HttpHeader> {
        headers(&self.0.headers)
    }

    async fn body(&self) -> &str {
        &self.0.body
    }

    async fn method(&self) -> &str {
        &self.0.method
    }

    /// The requested URL: an outbound request's absolute URL masked like the webhook's URL,
    /// an incoming request's path as received.
    async fn url(&self) -> String {
        match url::Url::parse(&self.0.url) {
            Ok(u) if u.host_str().is_some() => mask_url(&self.0.url),
            _ => self.0.url.clone(),
        }
    }

    async fn version(&self) -> &str {
        &self.0.version
    }
}

/// The response to a delivery attempt (body truncated to 64 KiB).
pub struct WebhookLogResponseNode(pub LoggedResponse);

#[Object(name = "WebhookLogResponse")]
impl WebhookLogResponseNode {
    async fn status_code(&self) -> i32 {
        self.0.status_code
    }

    async fn headers(&self) -> Vec<HttpHeader> {
        headers(&self.0.headers)
    }

    async fn body(&self) -> &str {
        &self.0.body
    }
}

/// The event a delivery attempt was for.
#[derive(SimpleObject)]
#[graphql(name = "OutboundWebhookJob")]
pub struct OutboundWebhookJobNode {
    event_type: String,
    scope: Option<String>,
    /// The JSON body sent.
    payload: String,
}

/// One delivery attempt.
pub struct OutboundWebhookLogNode(pub OutboundWebhookLog);

#[Object(name = "OutboundWebhookLog")]
impl OutboundWebhookLogNode {
    /// The opaque node ID.
    pub async fn id(&self) -> ID {
        ID(marshal_id(OUTBOUND_WEBHOOK_LOG_KIND, self.0.id))
    }

    /// The event.
    async fn job(&self, ctx: &Context<'_>) -> Result<OutboundWebhookJobNode> {
        let services = services(ctx)?;
        let store = OutboundWebhookStore::new(&services.db, services.keyring.clone());
        let job = store
            .job(self.0.job_id)
            .await
            .map_err(db_error)?
            .ok_or_else(|| error(codes::NOT_FOUND, "Outbound webhook job not found."))?
            .payload;
        let payload = store.payload(&job).map_err(store_error)?;
        Ok(OutboundWebhookJobNode {
            event_type: job.event_type,
            scope: job.scope,
            payload: String::from_utf8_lossy(&payload).into_owned(),
        })
    }

    async fn sent_at(&self) -> DateTime<Utc> {
        self.0.sent_at
    }

    /// The response status; 0 when no response arrived (see `error`).
    async fn status_code(&self) -> i32 {
        self.0.status_code
    }

    async fn request(&self) -> WebhookLogRequestNode {
        WebhookLogRequestNode(self.0.request.clone())
    }

    /// Null when no response arrived.
    async fn response(&self) -> Option<WebhookLogResponseNode> {
        self.0.response.clone().map(WebhookLogResponseNode)
    }

    /// Why the attempt failed, if it did.
    async fn error(&self) -> Option<&str> {
        self.0.error.as_deref()
    }
}

/// The channel a watch action posts through.
#[derive(Enum, Copy, Clone, Debug, PartialEq, Eq)]
#[graphql(name = "DeliveryChannel")]
pub enum DeliveryChannel {
    Webhook,
    SlackWebhook,
    MattermostWebhook,
    Telegram,
}

impl From<Channel> for DeliveryChannel {
    fn from(c: Channel) -> Self {
        match c {
            Channel::Webhook => DeliveryChannel::Webhook,
            Channel::SlackWebhook => DeliveryChannel::SlackWebhook,
            Channel::MattermostWebhook => DeliveryChannel::MattermostWebhook,
            Channel::Telegram => DeliveryChannel::Telegram,
        }
    }
}

/// One delivery attempt of a watch action through a channel. The destination URL may hold a
/// token, so only its host is shown.
pub struct ChannelDeliveryLogNode(pub ChannelDelivery);

#[Object(name = "ChannelDeliveryLog")]
impl ChannelDeliveryLogNode {
    /// The opaque node ID.
    async fn id(&self) -> ID {
        ID(marshal_id(CHANNEL_DELIVERY_LOG_KIND, self.0.log.id))
    }

    /// Null for a channel this version does not know.
    async fn channel(&self) -> Option<DeliveryChannel> {
        Channel::parse(&self.0.log.channel).map(Into::into)
    }

    /// The host posted to; null for Telegram (always the Bot API).
    async fn host(&self) -> Option<String> {
        if self.0.log.channel == Channel::Telegram.as_str() {
            return None;
        }
        url::Url::parse(&self.0.log.request.url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_owned))
    }

    async fn sent_at(&self) -> DateTime<Utc> {
        self.0.log.sent_at
    }

    /// The response status; 0 when no response arrived (see `error`).
    async fn status_code(&self) -> i32 {
        self.0.log.status_code
    }

    /// Why the attempt failed, if it did.
    async fn error(&self) -> Option<&str> {
        self.0.log.error.as_deref()
    }

    /// The watch's ID; null once the watch was deleted.
    async fn watch_id(&self) -> Option<ID> {
        self.0
            .monitor_id
            .map(|id| ID(marshal_id(sourcegull_monitors::MONITOR_KIND, id)))
    }

    /// The watch's description; null once the watch was deleted.
    async fn watch_description(&self) -> Option<&str> {
        self.0.monitor_description.as_deref()
    }
}

/// A page of channel delivery attempts.
#[derive(SimpleObject)]
#[graphql(name = "ChannelDeliveryLogConnection")]
pub struct ChannelDeliveryLogConnection {
    nodes: Vec<ChannelDeliveryLogNode>,
    total_count: i32,
    page_info: PageInfo,
}

/// Checks that the URL parses, plus the SSRF guard's check of IP literals.
fn validate_url(ctx: &Context<'_>, raw: &str) -> Result<()> {
    check_webhook_url(&services(ctx)?.config.load().value, raw).map_err(invalid)
}

fn event_types(input: Vec<ScopedEventTypeInput>) -> Result<Vec<ScopedEventType>> {
    if input.is_empty() {
        return Err(invalid("at least one event type is required"));
    }
    input
        .into_iter()
        .map(|t| {
            if !sourcegull_outbound_webhooks::events::is_known(&t.event_type) {
                return Err(invalid(format!("unknown event type {:?}", t.event_type)));
            }
            Ok(ScopedEventType {
                event_type: t.event_type,
                scope: t.scope.filter(|s| !s.is_empty()),
            })
        })
        .collect()
}

fn parse_id(id: &str) -> Result<i64> {
    unmarshal_int_id(OUTBOUND_WEBHOOK_KIND, id)
        .ok_or_else(|| invalid(format!("Invalid outbound webhook ID {id:?}.")))
}

/// The security event argument describing a webhook.
fn event_argument(w: &OutboundWebhook) -> serde_json::Value {
    json!({
        "id": w.id,
        "eventTypes": w.event_types.iter().map(|t| json!({
            "eventType": t.event_type,
            "scope": t.scope,
        })).collect::<Vec<_>>(),
    })
}

/// A node of this domain.
pub enum OutboundWebhooksNode {
    /// An outbound webhook.
    Webhook(OutboundWebhookNode),
    /// A delivery log.
    Log(OutboundWebhookLogNode),
}

/// The `node(id:)` loader of webhooks and delivery logs (site admins only).
pub fn node_loaders<N: From<OutboundWebhooksNode> + Send + 'static>() -> [NodeLoader<N>; 1] {
    fn load<'a, N: From<OutboundWebhooksNode> + Send + 'static>(
        ctx: &'a Context<'_>,
        kind: &'a str,
        id: &'a str,
    ) -> NodeFuture<'a, N> {
        Box::pin(async move { Ok(load_node(ctx, kind, id).await?.map(N::from)) })
    }
    [NodeLoader {
        kinds: &[OUTBOUND_WEBHOOK_KIND, OUTBOUND_WEBHOOK_LOG_KIND],
        load: load::<N>,
    }]
}

/// A webhook or delivery log by ID (site admins only).
async fn load_node(
    ctx: &Context<'_>,
    kind: &str,
    id: &str,
) -> Result<Option<OutboundWebhooksNode>> {
    require_site_admin(ctx)?;
    let Some(key) = unmarshal_int_id(kind, id) else {
        return Ok(None);
    };
    let services = services(ctx)?;
    let store = OutboundWebhookStore::new(&services.db, services.keyring.clone());
    Ok(if kind == OUTBOUND_WEBHOOK_KIND {
        store
            .get(key)
            .await
            .map_err(store_error)?
            .map(|w| OutboundWebhooksNode::Webhook(OutboundWebhookNode(w)))
    } else {
        store
            .log(key)
            .await
            .map_err(store_error)?
            .map(|l| OutboundWebhooksNode::Log(OutboundWebhookLogNode(l)))
    })
}

/// Outbound webhook queries.
#[derive(Default)]
pub struct OutboundWebhooksQuery;

#[Object]
impl OutboundWebhooksQuery {
    /// The webhooks, in creation order, optionally only those subscribed to one of
    /// `eventTypes` (site admins only).
    async fn outbound_webhooks(
        &self,
        ctx: &Context<'_>,
        #[graphql(default_with = "Some(20)")] first: Option<i32>,
        after: Option<String>,
        event_types: Option<Vec<ScopedEventTypeInput>>,
    ) -> Result<OutboundWebhookConnection> {
        require_site_admin(ctx)?;
        let limit = page_size(first, 20, MAX_PAGE);
        let offset = decode_cursor(after.as_deref())?;
        let services = services(ctx)?;
        let store = OutboundWebhookStore::new(&services.db, services.keyring.clone());
        let mut opts = ListOutboundWebhooks {
            event_types: event_types.map(|t| t.into_iter().map(|t| t.event_type).collect()),
            limit: limit + 1,
            offset,
        };
        let mut rows = store.list(&opts).await.map_err(store_error)?;
        opts.limit = 0;
        let total = store.count(&opts).await.map_err(db_error)?;
        let page_info = PageInfo::new(offset, limit, rows.len());
        rows.truncate(usize::try_from(limit).unwrap_or(0));
        Ok(OutboundWebhookConnection {
            nodes: rows.into_iter().map(OutboundWebhookNode).collect(),
            total_count: to_int(total),
            page_info,
        })
    }

    /// The delivery attempts of watch actions that post to a URL (webhook, Slack, Mattermost,
    /// Telegram), newest first; only failed ones with `onlyErrors` (site admins only).
    async fn channel_delivery_logs(
        &self,
        ctx: &Context<'_>,
        #[graphql(default_with = "Some(50)")] first: Option<i32>,
        after: Option<String>,
        #[graphql(default_with = "Some(false)")] only_errors: Option<bool>,
    ) -> Result<ChannelDeliveryLogConnection> {
        require_site_admin(ctx)?;
        let only_errors = only_errors.unwrap_or(false);
        let limit = page_size(first, 50, MAX_PAGE);
        let offset = decode_cursor(after.as_deref())?;
        let services = services(ctx)?;
        let store = OutboundWebhookStore::new(&services.db, services.keyring.clone());
        let mut logs = store
            .channel_deliveries(only_errors, limit + 1, offset)
            .await
            .map_err(store_error)?;
        let stats = store.channel_delivery_stats().await.map_err(db_error)?;
        let page_info = PageInfo::new(offset, limit, logs.len());
        logs.truncate(usize::try_from(limit).unwrap_or(0));
        Ok(ChannelDeliveryLogConnection {
            nodes: logs.into_iter().map(ChannelDeliveryLogNode).collect(),
            total_count: to_int(if only_errors {
                stats.errored
            } else {
                stats.total
            }),
            page_info,
        })
    }

    /// The event types webhooks can subscribe to (site admins only).
    async fn outbound_webhook_event_types(
        &self,
        ctx: &Context<'_>,
    ) -> Result<Vec<OutboundWebhookEventType>> {
        require_site_admin(ctx)?;
        Ok(EVENT_TYPES
            .iter()
            .map(|t| OutboundWebhookEventType {
                key: t.key.to_owned(),
                description: t.description.to_owned(),
            })
            .collect())
    }
}

/// Outbound webhook mutations.
#[derive(Default)]
pub struct OutboundWebhooksMutation;

#[Object]
impl OutboundWebhooksMutation {
    /// Creates a webhook (site admins only).
    async fn create_outbound_webhook(
        &self,
        ctx: &Context<'_>,
        input: OutboundWebhookCreateInput,
    ) -> Result<OutboundWebhookNode> {
        let actor = require_site_admin(ctx)?;
        validate_url(ctx, &input.url)?;
        if input.secret.is_empty() {
            return Err(invalid("secret cannot be empty"));
        }
        let types = event_types(input.event_types)?;
        let services = services(ctx)?;
        declared::ensure_creatable(services, declared::kinds::OUTBOUND_WEBHOOKS).await?;
        let webhook = OutboundWebhookStore::new(&services.db, services.keyring.clone())
            .create(&NewOutboundWebhook {
                url: input.url,
                secret: input.secret,
                event_types: types,
                created_by: Some(actor.user_id),
            })
            .await
            .map_err(store_error)?;
        audit::record(
            services,
            events::OUTBOUND_WEBHOOK_CREATED,
            Some(actor.user_id),
            event_argument(&webhook),
        )
        .await;
        Ok(OutboundWebhookNode(webhook))
    }

    /// Updates a webhook (site admins only).
    async fn update_outbound_webhook(
        &self,
        ctx: &Context<'_>,
        id: ID,
        input: OutboundWebhookUpdateInput,
    ) -> Result<OutboundWebhookNode> {
        let actor = require_site_admin(ctx)?;
        let key = parse_id(&id)?;
        if input.secret.as_deref() == Some("") {
            return Err(invalid("secret cannot be empty"));
        }
        let types = event_types(input.event_types)?;
        let services = services(ctx)?;
        declared::ensure_not_declared(
            services,
            declared::kinds::OUTBOUND_WEBHOOKS,
            &key.to_string(),
        )
        .await?;
        let store = OutboundWebhookStore::new(&services.db, services.keyring.clone());
        let stored = store
            .get(key)
            .await
            .map_err(store_error)?
            .ok_or_else(not_found)?;
        let url = keep_masked(&input.url, &stored.url, mask_url);
        validate_url(ctx, &url)?;
        let webhook = store
            .update(
                key,
                &OutboundWebhookUpdate {
                    url,
                    secret: input.secret,
                    event_types: types,
                    updated_by: Some(actor.user_id),
                },
            )
            .await
            .map_err(store_error)?
            .ok_or_else(not_found)?;
        audit::record(
            services,
            events::OUTBOUND_WEBHOOK_UPDATED,
            Some(actor.user_id),
            event_argument(&webhook),
        )
        .await;
        Ok(OutboundWebhookNode(webhook))
    }

    /// Deletes a webhook with its delivery logs (site admins only).
    async fn delete_outbound_webhook(&self, ctx: &Context<'_>, id: ID) -> Result<EmptyResponse> {
        let actor = require_site_admin(ctx)?;
        let key = parse_id(&id)?;
        let services = services(ctx)?;
        declared::ensure_not_declared(
            services,
            declared::kinds::OUTBOUND_WEBHOOKS,
            &key.to_string(),
        )
        .await?;
        let deleted = OutboundWebhookStore::new(&services.db, None)
            .delete(key)
            .await
            .map_err(db_error)?;
        if !deleted {
            return Err(not_found());
        }
        audit::record(
            services,
            events::OUTBOUND_WEBHOOK_DELETED,
            Some(actor.user_id),
            json!({ "id": key }),
        )
        .await;
        Ok(EmptyResponse)
    }
}
