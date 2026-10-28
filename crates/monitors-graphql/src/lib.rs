//! Watches: the `Monitor` type with its trigger query, actions and event logs,
//! `User.monitors`, `monitors` (site admins), and the create, update, toggle, delete, reset
//! and "send test" mutations.
//!
//! Who may see and manage a watch, and send its email, is `sourcegull_monitors::access`'s rule:
//! whoever may act for its owner (the user, the org's members, site admins).
//! The trigger query must be a `type:commit` or `type:diff` search. Watches run with the
//! owner's repository permissions (see `sourcegull_monitors::trigger`).

use std::sync::Arc;

use async_graphql::{Context, Enum, ID, InputObject, Object, Result, SimpleObject, Union};
use chrono::{DateTime, Utc};
use sourcegull_authn::Actor;
use sourcegull_db::Namespace;
use sourcegull_monitors::{
    Action, ActionEdit, ActionEvent, ActionKind, ActionSender, ActionSpec, ListMonitors, Monitor,
    MonitorStore, MonitorUpdate, NewMonitor, Notification, Priority, TriggerEvent,
};
use sourcegull_services::relay::{marshal_id, unmarshal_int_id};

mod feature;

pub use feature::MonitorsGraphql;

// `crate::context` and `crate::pagination`, as in the schema assembly.
use sourcegull_graphql_core::{context, pagination};

use crate::context::{
    access_error, codes, db_error, error, page_size, require_site_admin, require_viewer, services,
    to_int, validation, viewer,
};
use crate::pagination::{PageInfo, decode_cursor};
use sourcegull_graphql_core::EmptyResponse;
use sourcegull_graphql_core::events::log_backend;
use sourcegull_graphql_core::ids::parse_namespace_id;
use sourcegull_graphql_core::mask::{keep_masked, mask_tail, mask_url};
use sourcegull_graphql_core::node::{NodeFuture, NodeLoader};
use sourcegull_graphql_core::shared::{
    NamespaceRef, UserRef, load_namespace as load_namespace_value,
};

/// Node kind of watch IDs.
pub const MONITOR_KIND: &str = sourcegull_monitors::MONITOR_KIND;
/// Node kind of trigger query IDs (keyed by the watch id: one query per watch).
pub const TRIGGER_QUERY_KIND: &str = "CodeMonitorTriggerQuery";
/// Node kind of trigger event IDs.
pub const TRIGGER_EVENT_KIND: &str = "CodeMonitorTriggerEvent";
/// Node kinds of actions.
pub const EMAIL_KIND: &str = "CodeMonitorActionEmail";
pub const WEBHOOK_KIND: &str = "CodeMonitorActionWebhook";
pub const SLACK_WEBHOOK_KIND: &str = "CodeMonitorActionSlackWebhook";
pub const MATTERMOST_WEBHOOK_KIND: &str = "CodeMonitorActionMattermostWebhook";
pub const TELEGRAM_KIND: &str = "CodeMonitorActionTelegram";
/// Node kind of action event IDs.
pub const ACTION_EVENT_KIND: &str = "CodeMonitorActionEvent";
/// The kind of an action event that is a channel job of the outbound webhook queue.
pub const CHANNEL_ACTION_EVENT_KIND: &str = "CodeMonitorChannelActionEvent";

const MAX_DESCRIPTION_LEN: usize = 1024;
const MAX_QUERY_LEN: usize = 16 * 1024;
const MAX_ACTIONS: usize = 20;

fn store(ctx: &Context<'_>) -> Result<MonitorStore> {
    let s = services(ctx)?;
    Ok(MonitorStore::new(&s.db).with_keyring(s.keyring.clone()))
}

fn action_kind_name(kind: ActionKind) -> &'static str {
    match kind {
        ActionKind::Email => EMAIL_KIND,
        ActionKind::Webhook => WEBHOOK_KIND,
        ActionKind::SlackWebhook => SLACK_WEBHOOK_KIND,
        ActionKind::MattermostWebhook => MATTERMOST_WEBHOOK_KIND,
        ActionKind::Telegram => TELEGRAM_KIND,
    }
}

fn invalid_id(what: &str, id: &str) -> async_graphql::Error {
    error(codes::INVALID_INPUT, format!("Invalid {what} ID {id:?}."))
}

fn parse_monitor_id(id: &str) -> Result<i64> {
    unmarshal_int_id(MONITOR_KIND, id).ok_or_else(|| invalid_id("watch", id))
}

/// The status of a trigger or action event.
#[derive(Enum, Copy, Clone, Eq, PartialEq)]
pub enum EventStatus {
    Pending,
    Success,
    Error,
}

impl EventStatus {
    /// From a pgqueue state: an errored job will be retried.
    fn from_state(state: &str) -> Self {
        match state {
            "completed" => EventStatus::Success,
            "failed" | "canceled" => EventStatus::Error,
            _ => EventStatus::Pending,
        }
    }
}

/// `MonitorEmail.priority`.
#[derive(Enum, Copy, Clone, Eq, PartialEq)]
pub enum MonitorEmailPriority {
    Normal,
    Critical,
}

impl From<Priority> for MonitorEmailPriority {
    fn from(p: Priority) -> Self {
        match p {
            Priority::Normal => MonitorEmailPriority::Normal,
            Priority::Critical => MonitorEmailPriority::Critical,
        }
    }
}

impl From<MonitorEmailPriority> for Priority {
    fn from(p: MonitorEmailPriority) -> Self {
        match p {
            MonitorEmailPriority::Normal => Priority::Normal,
            MonitorEmailPriority::Critical => Priority::Critical,
        }
    }
}

/// A watch with one trigger and possibly many actions.
#[derive(Debug, Clone)]
pub struct MonitorNode(pub Monitor);

#[Object(name = "Monitor")]
impl MonitorNode {
    /// The opaque node ID.
    pub async fn id(&self) -> ID {
        ID(marshal_id(MONITOR_KIND, self.0.id))
    }

    /// The user who created the watch (null once deleted).
    async fn created_by(&self, ctx: &Context<'_>) -> Result<Option<UserRef>> {
        let Some(id) = self.0.created_by else {
            return Ok(None);
        };
        Ok(services(ctx)?
            .db
            .users()
            .get_by_id(id)
            .await
            .map_err(db_error)?
            .map(UserRef))
    }

    async fn created_at(&self) -> DateTime<Utc> {
        self.0.created_at
    }

    async fn description(&self) -> &str {
        &self.0.description
    }

    /// The user whose repository permissions the watch searches with: the owner, or for an
    /// organization's watch the member who last saved, enabled or disabled it. Null when there
    /// is none (the watch then runs nothing).
    async fn runs_as(&self, ctx: &Context<'_>) -> Result<Option<UserRef>> {
        let db = &services(ctx)?.db;
        let Some(actor) = sourcegull_monitors::trigger::owner_actor(db, &self.0)
            .await
            .map_err(db_error)?
        else {
            return Ok(None);
        };
        Ok(db
            .users()
            .get_by_id(actor.user_id)
            .await
            .map_err(db_error)?
            .map(UserRef))
    }

    /// Whether every email recipient gets all results, found with the access of `runsAs`,
    /// instead of only the results in repositories they can read. False while `runsAs` may
    /// not make a watch unfiltered.
    async fn unfiltered_results(&self, ctx: &Context<'_>) -> Result<bool> {
        let s = services(ctx)?;
        Ok(
            sourcegull_monitors::access::unfiltered_owner(&s.db, &s.config.load().value, &self.0)
                .await
                .map_err(db_error)?
                .is_some(),
        )
    }

    /// Who made the watch unfiltered (null when it is not, or once deleted).
    async fn unfiltered_by(&self, ctx: &Context<'_>) -> Result<Option<UserRef>> {
        let Some(id) = self.0.unfiltered_by else {
            return Ok(None);
        };
        Ok(services(ctx)?
            .db
            .users()
            .get_by_id(id)
            .await
            .map_err(db_error)?
            .map(UserRef))
    }

    /// When the watch was made unfiltered.
    async fn unfiltered_at(&self) -> Option<DateTime<Utc>> {
        self.0.unfiltered_at
    }

    /// Owners can edit the watch.
    async fn owner(&self, ctx: &Context<'_>) -> Result<NamespaceRef> {
        load_namespace(ctx, self.0.namespace)
            .await?
            .ok_or_else(|| error(codes::NOT_FOUND, "The owner no longer exists."))
    }

    async fn enabled(&self) -> bool {
        self.0.enabled
    }

    /// The trigger: its query.
    async fn trigger(&self) -> MonitorTrigger {
        MonitorTrigger::MonitorQuery(MonitorQueryNode(self.0.clone()))
    }

    /// The actions run when the trigger finds results.
    async fn actions(
        &self,
        ctx: &Context<'_>,
        #[graphql(default = 50)] first: i32,
        after: Option<String>,
    ) -> Result<MonitorActionConnection> {
        action_connection(ctx, self.0.id, None, first, after).await
    }

    /// The watch's page.
    async fn url(&self) -> String {
        format!("/watches/{}", marshal_id(MONITOR_KIND, self.0.id))
    }
}

async fn action_connection(
    ctx: &Context<'_>,
    monitor_id: i64,
    trigger_job: Option<i64>,
    first: i32,
    after: Option<String>,
) -> Result<MonitorActionConnection> {
    let limit = page_size(Some(first), 50, 1000);
    let offset = decode_cursor(after.as_deref())?;
    let all = store(ctx)?.actions(monitor_id).await.map_err(db_error)?;
    let total = all.len();
    let start = usize::try_from(offset).unwrap_or(usize::MAX).min(total);
    let page: Vec<Action> = all
        .into_iter()
        .skip(start)
        .take(usize::try_from(limit).unwrap_or(0) + 1)
        .collect();
    let page_info = PageInfo::new(offset, limit, page.len());
    let nodes = page
        .into_iter()
        .take(usize::try_from(limit).unwrap_or(0))
        .map(|a| MonitorAction::new(a, trigger_job))
        .collect();
    Ok(MonitorActionConnection {
        nodes,
        total_count: to_int(i64::try_from(total).unwrap_or(i64::MAX)),
        page_info,
    })
}

/// Supported triggers for watches.
#[derive(Union)]
pub enum MonitorTrigger {
    MonitorQuery(MonitorQueryNode),
}

/// A query that can serve as a trigger for watches.
pub struct MonitorQueryNode(Monitor);

#[Object(name = "MonitorQuery")]
impl MonitorQueryNode {
    async fn id(&self) -> ID {
        ID(marshal_id(TRIGGER_QUERY_KIND, self.0.id))
    }

    async fn query(&self) -> &str {
        &self.0.query
    }

    /// The trigger's runs, newest first: each searched the commits that ref updates added.
    async fn events(
        &self,
        ctx: &Context<'_>,
        #[graphql(default = 50)] first: i32,
        after: Option<String>,
    ) -> Result<MonitorTriggerEventConnection> {
        let limit = page_size(Some(first), 50, 1000);
        let offset = decode_cursor(after.as_deref())?;
        let (mut events, total) = store(ctx)?
            .trigger_events(self.0.id, limit + 1, offset)
            .await
            .map_err(db_error)?;
        // Unless the watch is unfiltered, the viewer sees only the repositories they can read.
        let s = services(ctx)?;
        let unfiltered =
            sourcegull_monitors::access::unfiltered_owner(&s.db, &s.config.load().value, &self.0)
                .await
                .map_err(db_error)?
                .is_some();
        if !unfiltered {
            let access = s.authz.access(viewer(ctx)).await.map_err(db_error)?;
            for e in &mut events {
                e.ranges
                    .retain(|r| access.can_read(sourcegull_types::RepoId(r.repo_id)));
                if let Some(results) = &mut e.results {
                    results.retain(|r| access.can_read(sourcegull_types::RepoId(r.repository_id)));
                    e.num_results = i32::try_from(results.len()).unwrap_or(i32::MAX);
                }
            }
        }
        let page_info = PageInfo::new(offset, limit, events.len());
        let nodes = events
            .into_iter()
            .take(usize::try_from(limit).unwrap_or(0))
            .map(MonitorTriggerEventNode)
            .collect();
        Ok(MonitorTriggerEventConnection {
            nodes,
            total_count: to_int(total),
            page_info,
        })
    }
}

/// A list of trigger events.
#[derive(SimpleObject)]
pub struct MonitorTriggerEventConnection {
    pub nodes: Vec<MonitorTriggerEventNode>,
    pub total_count: i32,
    pub page_info: PageInfo,
}

/// A commit range a trigger event searched.
#[derive(SimpleObject)]
pub struct MonitorCommitRange {
    /// The repository's name.
    pub repository: String,
    /// The watched revision; empty for the default branch.
    pub revision: String,
    /// The last commit searched before; null when the whole history was searched.
    pub from: Option<String>,
    /// The head searched up to.
    pub to: String,
}

/// One run of a trigger, with the actions it ran.
pub struct MonitorTriggerEventNode(TriggerEvent);

#[Object(name = "MonitorTriggerEvent")]
impl MonitorTriggerEventNode {
    async fn id(&self) -> ID {
        ID(marshal_id(TRIGGER_EVENT_KIND, self.0.id))
    }

    async fn status(&self) -> EventStatus {
        EventStatus::from_state(&self.0.state)
    }

    /// Why the run failed (or why it is being retried).
    async fn message(&self) -> Option<&str> {
        self.0.failure_message.as_deref()
    }

    async fn timestamp(&self) -> DateTime<Utc> {
        self.0.finished_at.unwrap_or(self.0.queued_at)
    }

    /// The query the run searched with.
    async fn query(&self) -> Option<&str> {
        Some(&self.0.query)
    }

    /// The number of results found; zero until the run succeeded.
    async fn result_count(&self) -> i32 {
        self.0.num_results
    }

    /// The commit ranges searched.
    async fn ranges(&self) -> Vec<MonitorCommitRange> {
        self.0
            .ranges
            .iter()
            .map(|r| MonitorCommitRange {
                repository: r.repo.clone(),
                revision: r.rev.clone(),
                from: r.from.clone(),
                to: r.to.clone(),
            })
            .collect()
    }

    /// The watch's actions, each with its events for this run.
    async fn actions(
        &self,
        ctx: &Context<'_>,
        #[graphql(default = 50)] first: i32,
        after: Option<String>,
    ) -> Result<MonitorActionConnection> {
        action_connection(ctx, self.0.monitor_id, Some(self.0.id), first, after).await
    }
}

/// A list of actions.
#[derive(SimpleObject)]
pub struct MonitorActionConnection {
    pub nodes: Vec<MonitorAction>,
    pub total_count: i32,
    pub page_info: PageInfo,
}

/// Supported actions for watches.
// Variant names are the GraphQL type names.
#[allow(clippy::enum_variant_names)]
#[derive(Union)]
pub enum MonitorAction {
    MonitorEmail(MonitorEmailNode),
    MonitorWebhook(MonitorWebhookNode),
    MonitorSlackWebhook(MonitorSlackWebhookNode),
    MonitorMattermostWebhook(MonitorMattermostWebhookNode),
    MonitorTelegram(MonitorTelegramNode),
}

impl MonitorAction {
    fn new(a: Action, trigger_job: Option<i64>) -> Self {
        let node = ActionNode {
            action: a,
            trigger_job,
        };
        match node.action.spec.kind {
            ActionKind::Email => MonitorAction::MonitorEmail(MonitorEmailNode(node)),
            ActionKind::Webhook => MonitorAction::MonitorWebhook(MonitorWebhookNode(node)),
            ActionKind::SlackWebhook => {
                MonitorAction::MonitorSlackWebhook(MonitorSlackWebhookNode(node))
            }
            ActionKind::MattermostWebhook => {
                MonitorAction::MonitorMattermostWebhook(MonitorMattermostWebhookNode(node))
            }
            ActionKind::Telegram => MonitorAction::MonitorTelegram(MonitorTelegramNode(node)),
        }
    }
}

/// An action, with its events limited to one trigger event when listed under it.
pub struct ActionNode {
    action: Action,
    trigger_job: Option<i64>,
}

impl ActionNode {
    fn id(&self) -> ID {
        ID(marshal_id(
            action_kind_name(self.action.spec.kind),
            self.action.id,
        ))
    }

    async fn events(
        &self,
        ctx: &Context<'_>,
        first: i32,
        after: Option<String>,
    ) -> Result<MonitorActionEventConnection> {
        let limit = page_size(Some(first), 50, 1000);
        let offset = decode_cursor(after.as_deref())?;
        let (events, total) = store(ctx)?
            .action_events(Some(self.action.id), self.trigger_job, limit + 1, offset)
            .await
            .map_err(db_error)?;
        let page_info = PageInfo::new(offset, limit, events.len());
        let nodes = events
            .into_iter()
            .take(usize::try_from(limit).unwrap_or(0))
            .map(MonitorActionEventNode)
            .collect();
        Ok(MonitorActionEventConnection {
            nodes,
            total_count: to_int(total),
            page_info,
        })
    }
}

/// Email is one of the supported actions of watches.
pub struct MonitorEmailNode(ActionNode);

#[Object(name = "MonitorEmail")]
impl MonitorEmailNode {
    async fn id(&self) -> ID {
        self.0.id()
    }

    async fn enabled(&self) -> bool {
        self.0.action.spec.enabled
    }

    /// Whether to include the result contents in the email message.
    async fn include_results(&self) -> bool {
        self.0.action.spec.include_results
    }

    async fn priority(&self) -> MonitorEmailPriority {
        self.0.action.spec.priority.into()
    }

    /// Use header to automatically approve the message in a read-only or moderated mailing
    /// list: one `Name: value` header field (e.g. `Approved: password`) added to every email.
    /// Masked: the field name and the value's last characters (`Approved: …word`).
    async fn header(&self) -> String {
        mask_header(&self.0.action.spec.header)
    }

    /// The users and organizations (every member) that receive the email.
    async fn recipients(
        &self,
        ctx: &Context<'_>,
        #[graphql(default = 50)] first: i32,
        after: Option<String>,
    ) -> Result<MonitorActionEmailRecipientsConnection> {
        let limit = usize::try_from(page_size(Some(first), 50, 1000)).unwrap_or(0);
        let offset = decode_cursor(after.as_deref())?;
        let all = &self.0.action.spec.recipients;
        let start = usize::try_from(offset).unwrap_or(usize::MAX).min(all.len());
        let mut nodes = Vec::new();
        for ns in all.iter().skip(start).take(limit) {
            if let Some(n) = load_namespace(ctx, *ns).await? {
                nodes.push(n);
            }
        }
        let fetched = all.len().saturating_sub(start).min(limit + 1);
        Ok(MonitorActionEmailRecipientsConnection {
            nodes,
            total_count: to_int(i64::try_from(all.len()).unwrap_or(i64::MAX)),
            page_info: PageInfo::new(offset, i64::try_from(limit).unwrap_or(0), fetched),
        })
    }

    async fn events(
        &self,
        ctx: &Context<'_>,
        #[graphql(default = 50)] first: i32,
        after: Option<String>,
    ) -> Result<MonitorActionEventConnection> {
        self.0.events(ctx, first, after).await
    }
}

/// Webhook is one of the supported actions of watches.
pub struct MonitorWebhookNode(ActionNode);

#[Object(name = "MonitorWebhook")]
impl MonitorWebhookNode {
    async fn id(&self) -> ID {
        self.0.id()
    }

    async fn enabled(&self) -> bool {
        self.0.action.spec.enabled
    }

    /// Whether to include the results in the webhook payload.
    async fn include_results(&self) -> bool {
        self.0.action.spec.include_results
    }

    /// The endpoint the webhook event will be sent to, masked: the scheme and host, and the
    /// last characters of a long rest.
    async fn url(&self) -> String {
        mask_url(&self.0.action.spec.url)
    }

    async fn events(
        &self,
        ctx: &Context<'_>,
        #[graphql(default = 50)] first: i32,
        after: Option<String>,
    ) -> Result<MonitorActionEventConnection> {
        self.0.events(ctx, first, after).await
    }
}

/// SlackWebhook is one of the supported actions of watches.
pub struct MonitorSlackWebhookNode(ActionNode);

#[Object(name = "MonitorSlackWebhook")]
impl MonitorSlackWebhookNode {
    async fn id(&self) -> ID {
        self.0.id()
    }

    async fn enabled(&self) -> bool {
        self.0.action.spec.enabled
    }

    /// Whether to include the results in the Slack message.
    async fn include_results(&self) -> bool {
        self.0.action.spec.include_results
    }

    /// The incoming webhook URL (Slack, Rocket.Chat), masked as `MonitorWebhook.url`.
    async fn url(&self) -> String {
        mask_url(&self.0.action.spec.url)
    }

    async fn events(
        &self,
        ctx: &Context<'_>,
        #[graphql(default = 50)] first: i32,
        after: Option<String>,
    ) -> Result<MonitorActionEventConnection> {
        self.0.events(ctx, first, after).await
    }
}

/// MattermostWebhook is one of the supported actions of watches.
pub struct MonitorMattermostWebhookNode(ActionNode);

#[Object(name = "MonitorMattermostWebhook")]
impl MonitorMattermostWebhookNode {
    async fn id(&self) -> ID {
        self.0.id()
    }

    async fn enabled(&self) -> bool {
        self.0.action.spec.enabled
    }

    /// Whether to include the results in the Mattermost message.
    async fn include_results(&self) -> bool {
        self.0.action.spec.include_results
    }

    /// The Mattermost incoming webhook URL, masked as `MonitorWebhook.url`.
    async fn url(&self) -> String {
        mask_url(&self.0.action.spec.url)
    }

    async fn events(
        &self,
        ctx: &Context<'_>,
        #[graphql(default = 50)] first: i32,
        after: Option<String>,
    ) -> Result<MonitorActionEventConnection> {
        self.0.events(ctx, first, after).await
    }
}

/// Telegram is one of the supported actions of watches: a bot posts to a chat.
pub struct MonitorTelegramNode(ActionNode);

#[Object(name = "MonitorTelegram")]
impl MonitorTelegramNode {
    async fn id(&self) -> ID {
        self.0.id()
    }

    async fn enabled(&self) -> bool {
        self.0.action.spec.enabled
    }

    /// Whether to include the results in the Telegram message.
    async fn include_results(&self) -> bool {
        self.0.action.spec.include_results
    }

    /// The token of the bot that sends the messages, masked: only its last characters.
    async fn bot_token(&self) -> String {
        mask_token(&self.0.action.spec.url)
    }

    /// The chat the bot posts to: a numeric chat ID or a public channel's `@username`.
    async fn chat_id(&self) -> &str {
        &self.0.action.spec.chat_id
    }

    async fn events(
        &self,
        ctx: &Context<'_>,
        #[graphql(default = 50)] first: i32,
        after: Option<String>,
    ) -> Result<MonitorActionEventConnection> {
        self.0.events(ctx, first, after).await
    }
}

/// A list of email recipients.
#[derive(SimpleObject)]
pub struct MonitorActionEmailRecipientsConnection {
    pub nodes: Vec<NamespaceRef>,
    pub total_count: i32,
    pub page_info: PageInfo,
}

/// A list of action events.
#[derive(SimpleObject)]
pub struct MonitorActionEventConnection {
    pub nodes: Vec<MonitorActionEventNode>,
    pub total_count: i32,
    pub page_info: PageInfo,
}

/// One delivery of a run's results through an action.
pub struct MonitorActionEventNode(ActionEvent);

#[Object(name = "MonitorActionEvent")]
impl MonitorActionEventNode {
    async fn id(&self) -> ID {
        let kind = if self.0.channel_job {
            CHANNEL_ACTION_EVENT_KIND
        } else {
            ACTION_EVENT_KIND
        };
        ID(marshal_id(kind, self.0.id))
    }

    async fn status(&self) -> EventStatus {
        EventStatus::from_state(&self.0.state)
    }

    /// Why the delivery failed (or why it is being retried).
    async fn message(&self) -> Option<&str> {
        self.0.failure_message.as_deref()
    }

    async fn timestamp(&self) -> DateTime<Utc> {
        self.0.finished_at.unwrap_or(self.0.queued_at)
    }
}

/// A list of watches.
#[derive(SimpleObject)]
pub struct MonitorConnection {
    pub nodes: Vec<MonitorNode>,
    pub total_count: i32,
    pub page_info: PageInfo,
}

/// Watches of `affiliated_user` (theirs and their orgs'), or all of them.
pub async fn list(
    ctx: &Context<'_>,
    affiliated_user: Option<sourcegull_types::UserId>,
    first: i32,
    after: Option<String>,
) -> Result<MonitorConnection> {
    let limit = page_size(Some(first), 50, 1000);
    let offset = decode_cursor(after.as_deref())?;
    let (monitors, total) = store(ctx)?
        .list(&ListMonitors {
            affiliated_user,
            limit: limit + 1,
            offset,
        })
        .await
        .map_err(db_error)?;
    let page_info = PageInfo::new(offset, limit, monitors.len());
    Ok(MonitorConnection {
        nodes: monitors
            .into_iter()
            .take(usize::try_from(limit).unwrap_or(0))
            .map(MonitorNode)
            .collect(),
        total_count: to_int(total),
        page_info,
    })
}

/// `User.monitors`: the user themself or site admins only.
pub async fn user_monitors(
    ctx: &Context<'_>,
    user: sourcegull_types::UserId,
    first: i32,
    after: Option<String>,
) -> Result<MonitorConnection> {
    sourcegull_monitors::access::require_can_list_user(viewer(ctx), user).map_err(access_error)?;
    list(ctx, Some(user), first, after).await
}

/// Loads a watch for `node(id:)`: visible to whoever may act for its owner.
pub async fn load_node(ctx: &Context<'_>, id: i64) -> Result<Option<MonitorNode>> {
    let Some(actor) = viewer(ctx) else {
        return Ok(None);
    };
    let Some(m) = store(ctx)?.get(id).await.map_err(db_error)? else {
        return Ok(None);
    };
    Ok(
        sourcegull_monitors::access::can_view(&services(ctx)?.db, Some(actor), &m)
            .await
            .map_err(db_error)?
            .then_some(MonitorNode(m)),
    )
}

/// Loads a live namespace, or `None`.
async fn load_namespace(ctx: &Context<'_>, ns: Namespace) -> Result<Option<NamespaceRef>> {
    Ok(load_namespace_value(ctx, ns).await?.map(NamespaceRef))
}

/// The `node(id:)` loader of watches, for the schema assembly's node type `N`.
pub fn node_loaders<N: From<MonitorNode> + Send + 'static>() -> [NodeLoader<N>; 1] {
    fn load<'a, N: From<MonitorNode> + Send + 'static>(
        ctx: &'a Context<'_>,
        _kind: &'a str,
        id: &'a str,
    ) -> NodeFuture<'a, N> {
        Box::pin(async move {
            let Some(key) = unmarshal_int_id(MONITOR_KIND, id) else {
                return Ok(None);
            };
            Ok(load_node(ctx, key).await?.map(N::from))
        })
    }
    [NodeLoader {
        kinds: &[MONITOR_KIND],
        load: load::<N>,
    }]
}

async fn require_can_own(ctx: &Context<'_>, actor: &Actor, ns: Namespace) -> Result<()> {
    sourcegull_monitors::access::require_can_own(&services(ctx)?.db, actor, ns)
        .await
        .map_err(access_error)
}

/// Loads a watch the viewer may manage: `NOT_FOUND` otherwise (as for a missing one).
async fn load_for_admin(ctx: &Context<'_>, actor: &Actor, id: i64) -> Result<Monitor> {
    let m = store(ctx)?
        .get(id)
        .await
        .map_err(db_error)?
        .ok_or_else(|| access_error(sourcegull_monitors::access::not_found()))?;
    sourcegull_monitors::access::require_can_manage(&services(ctx)?.db, actor, &m)
        .await
        .map_err(access_error)?;
    Ok(m)
}

/// The input required to create a watch.
#[derive(InputObject)]
pub struct MonitorInput {
    /// The owner: a user or an organization.
    pub namespace: ID,
    pub description: String,
    pub enabled: bool,
    /// Send every email recipient all results, found with your access, including results in
    /// repositories they cannot read. Site admins only, unless
    /// `watches.allowUnfilteredResults` is on.
    #[graphql(default)]
    pub unfiltered_results: bool,
}

/// The input required to edit a watch.
#[derive(InputObject)]
pub struct MonitorEditInput {
    pub id: ID,
    pub update: MonitorInput,
}

/// The input required to create a trigger.
#[derive(InputObject)]
pub struct MonitorTriggerInput {
    pub query: String,
}

/// The input required to edit a trigger.
#[derive(InputObject)]
pub struct MonitorEditTriggerInput {
    pub id: ID,
    pub update: MonitorTriggerInput,
}

/// The input required to create an email action.
#[derive(InputObject, Clone)]
pub struct MonitorEmailInput {
    pub enabled: bool,
    pub include_results: bool,
    pub priority: MonitorEmailPriority,
    /// Users or organizations (every member) that receive the email.
    pub recipients: Vec<ID>,
    /// Use header to automatically approve the message in a read-only or moderated mailing
    /// list: empty, or one `Name: value` header field (e.g. `Approved: password`).
    /// On an update of a stored action, empty or the masked header keeps the stored one.
    pub header: String,
    /// On an update of a stored action, removes its header (`header` is ignored).
    #[graphql(default)]
    pub remove_header: bool,
}

/// The input required to create a webhook action.
#[derive(InputObject, Clone)]
pub struct MonitorWebhookInput {
    pub enabled: bool,
    pub include_results: bool,
    /// On an update of a stored action, empty or the masked URL keeps the stored one.
    pub url: String,
}

/// The input required to create a Slack webhook action.
#[derive(InputObject, Clone)]
pub struct MonitorSlackWebhookInput {
    pub enabled: bool,
    pub include_results: bool,
    /// On an update of a stored action, empty or the masked URL keeps the stored one.
    pub url: String,
}

/// The input required to create a Mattermost webhook action.
#[derive(InputObject, Clone)]
pub struct MonitorMattermostWebhookInput {
    pub enabled: bool,
    pub include_results: bool,
    /// On an update of a stored action, empty or the masked URL keeps the stored one.
    pub url: String,
}

/// The input required to create a Telegram action.
#[derive(InputObject, Clone)]
pub struct MonitorTelegramInput {
    pub enabled: bool,
    pub include_results: bool,
    /// The bot's token, from BotFather (`123456:ABC-...`).
    /// On an update of a stored action, empty or the masked token keeps the stored one.
    pub bot_token: String,
    /// The chat the bot posts to: a numeric chat ID (negative for groups and channels) or a
    /// public channel's `@username`. The bot must be a member of the chat.
    pub chat_id: String,
}

/// The input required to create an action: exactly one field set.
#[derive(InputObject)]
pub struct MonitorActionInput {
    pub email: Option<MonitorEmailInput>,
    pub webhook: Option<MonitorWebhookInput>,
    pub slack_webhook: Option<MonitorSlackWebhookInput>,
    pub mattermost_webhook: Option<MonitorMattermostWebhookInput>,
    pub telegram: Option<MonitorTelegramInput>,
}

/// The input required to edit an email action (no `id` creates one).
#[derive(InputObject)]
pub struct MonitorEditEmailInput {
    pub id: Option<ID>,
    pub update: MonitorEmailInput,
}

#[derive(InputObject)]
pub struct MonitorEditWebhookInput {
    pub id: Option<ID>,
    pub update: MonitorWebhookInput,
}

#[derive(InputObject)]
pub struct MonitorEditSlackWebhookInput {
    pub id: Option<ID>,
    pub update: MonitorSlackWebhookInput,
}

#[derive(InputObject)]
pub struct MonitorEditMattermostWebhookInput {
    pub id: Option<ID>,
    pub update: MonitorMattermostWebhookInput,
}

#[derive(InputObject)]
pub struct MonitorEditTelegramInput {
    pub id: Option<ID>,
    pub update: MonitorTelegramInput,
}

/// The input required to edit an action: exactly one field set.
#[derive(InputObject)]
pub struct MonitorEditActionInput {
    pub email: Option<MonitorEditEmailInput>,
    pub webhook: Option<MonitorEditWebhookInput>,
    pub slack_webhook: Option<MonitorEditSlackWebhookInput>,
    pub mattermost_webhook: Option<MonitorEditMattermostWebhookInput>,
    pub telegram: Option<MonitorEditTelegramInput>,
}

/// Checks a webhook URL: absolute http(s).
fn check_url(url: &str, problems: &mut Vec<String>) {
    match url::Url::parse(url) {
        Ok(u) if matches!(u.scheme(), "http" | "https") && u.host().is_some() => {}
        _ => problems.push(format!("The webhook URL {url:?} is not an http(s) URL.")),
    }
}

/// A Telegram bot token as reads return it: its last characters.
fn mask_token(token: &str) -> String {
    mask_tail(token)
}

/// An email header as reads return it: the field name and the masked value
/// (`Approved: …word`).
fn mask_header(header: &str) -> String {
    match header.split_once(':') {
        _ if header.is_empty() => String::new(),
        Some((name, value)) => format!("{}: {}", name.trim(), mask_tail(value.trim())),
        None => mask_tail(header),
    }
}

/// The header of an edited or tested email action: `input`, or the stored action's header
/// when `input` is empty or that header's masked form; empty with `remove`.
fn keep_header(input: &str, remove: bool, stored: Option<&Action>) -> String {
    if remove {
        return String::new();
    }
    match stored.filter(|a| a.spec.kind == ActionKind::Email) {
        Some(a) => keep_masked(input, &a.spec.header, mask_header),
        None => input.trim().to_owned(),
    }
}

/// The secret of an edited or tested action: `input`, or the stored action's secret when
/// `input` is empty or that secret's masked form.
fn keep_secret(input: &str, kind: ActionKind, stored: Option<&Action>) -> String {
    let Some(stored) = stored.filter(|a| a.spec.kind == kind) else {
        return input.trim().to_owned();
    };
    if kind == ActionKind::Telegram {
        keep_masked(input, &stored.spec.url, mask_token)
    } else {
        keep_masked(input, &stored.spec.url, mask_url)
    }
}

/// The settings of an email action; `stored` is the action it edits or tests, whose header
/// [`keep_header`] keeps.
async fn email_spec(
    ctx: &Context<'_>,
    actor: &Actor,
    e: &MonitorEmailInput,
    stored: Option<&Action>,
    problems: &mut Vec<String>,
) -> Result<ActionSpec> {
    let mut recipients = Vec::new();
    if e.recipients.is_empty() {
        problems.push("An email action needs at least one recipient.".to_owned());
    }
    let header = keep_header(&e.header, e.remove_header, stored);
    if let Err(problem) = sourcegull_monitors::parse_email_header(&header) {
        problems.push(problem);
    }
    for r in &e.recipients {
        let ns = parse_namespace_id(r)?;
        if load_namespace(ctx, ns).await?.is_none() {
            return Err(error(codes::NOT_FOUND, "Recipient not found."));
        }
        sourcegull_monitors::access::require_can_email(&services(ctx)?.db, actor, ns)
            .await
            .map_err(access_error)?;
        if !recipients.contains(&ns) {
            recipients.push(ns);
        }
    }
    Ok(ActionSpec {
        kind: ActionKind::Email,
        enabled: e.enabled,
        include_results: e.include_results,
        url: String::new(),
        chat_id: String::new(),
        priority: e.priority.into(),
        header,
        recipients,
    })
}

fn webhook_spec(
    kind: ActionKind,
    enabled: bool,
    include_results: bool,
    url: &str,
    problems: &mut Vec<String>,
) -> ActionSpec {
    let url = url.trim();
    check_url(url, problems);
    ActionSpec {
        kind,
        enabled,
        include_results,
        url: url.to_owned(),
        chat_id: String::new(),
        priority: Priority::Normal,
        header: String::new(),
        recipients: Vec::new(),
    }
}

/// Whether `token` has the form of a Telegram bot token: `<bot id>:<secret>`.
fn is_bot_token(token: &str) -> bool {
    token.split_once(':').is_some_and(|(id, secret)| {
        !id.is_empty()
            && id.bytes().all(|b| b.is_ascii_digit())
            && !secret.is_empty()
            && secret
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    })
}

/// Whether `chat` is a Telegram chat ID (an integer) or a public channel's `@username`.
fn is_chat_id(chat: &str) -> bool {
    if let Some(name) = chat.strip_prefix('@') {
        return (4..=32).contains(&name.len())
            && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
    }
    let digits = chat.strip_prefix('-').unwrap_or(chat);
    !digits.is_empty() && digits.len() <= 20 && digits.bytes().all(|b| b.is_ascii_digit())
}

fn telegram_spec(t: &MonitorTelegramInput, problems: &mut Vec<String>) -> ActionSpec {
    let token = t.bot_token.trim();
    let chat = t.chat_id.trim();
    if !is_bot_token(token) {
        problems.push("The Telegram bot token is not of the form `123456:ABC-...`.".to_owned());
    }
    if !is_chat_id(chat) {
        problems.push(format!(
            "The Telegram chat {chat:?} is not a numeric chat ID or an `@channel` username."
        ));
    }
    ActionSpec {
        kind: ActionKind::Telegram,
        enabled: t.enabled,
        include_results: t.include_results,
        url: token.to_owned(),
        chat_id: chat.to_owned(),
        priority: Priority::Normal,
        header: String::new(),
        recipients: Vec::new(),
    }
}

fn one_of(n: usize, problems: &mut Vec<String>) {
    if n != 1 {
        problems.push(
            "Each action must set exactly one of `email`, `webhook`, `slackWebhook`, \
             `mattermostWebhook` and `telegram`."
                .to_owned(),
        );
    }
}

/// The settings of a created action; `None` when no field is set (reported by [`one_of`]).
async fn action_spec(
    ctx: &Context<'_>,
    actor: &Actor,
    a: &MonitorActionInput,
    problems: &mut Vec<String>,
) -> Result<Option<ActionSpec>> {
    let set = usize::from(a.email.is_some())
        + usize::from(a.webhook.is_some())
        + usize::from(a.slack_webhook.is_some())
        + usize::from(a.mattermost_webhook.is_some())
        + usize::from(a.telegram.is_some());
    one_of(set, problems);
    Ok(if let Some(e) = &a.email {
        Some(email_spec(ctx, actor, e, None, problems).await?)
    } else if let Some(w) = &a.webhook {
        Some(webhook_spec(
            ActionKind::Webhook,
            w.enabled,
            w.include_results,
            &w.url,
            problems,
        ))
    } else if let Some(w) = &a.slack_webhook {
        Some(webhook_spec(
            ActionKind::SlackWebhook,
            w.enabled,
            w.include_results,
            &w.url,
            problems,
        ))
    } else if let Some(w) = &a.mattermost_webhook {
        Some(webhook_spec(
            ActionKind::MattermostWebhook,
            w.enabled,
            w.include_results,
            &w.url,
            problems,
        ))
    } else {
        a.telegram.as_ref().map(|t| telegram_spec(t, problems))
    })
}

/// An action of an update; `None` when no field is set (reported by [`one_of`]). A stored
/// action (`stored`, the watch's actions) keeps its URL, bot token or email header when the
/// input leaves it empty or sends its masked form.
async fn action_edit(
    ctx: &Context<'_>,
    actor: &Actor,
    a: &MonitorEditActionInput,
    stored: &[Action],
    problems: &mut Vec<String>,
) -> Result<Option<ActionEdit>> {
    let set = usize::from(a.email.is_some())
        + usize::from(a.webhook.is_some())
        + usize::from(a.slack_webhook.is_some())
        + usize::from(a.mattermost_webhook.is_some())
        + usize::from(a.telegram.is_some());
    one_of(set, problems);
    let (kind, id) = if let Some(e) = &a.email {
        (ActionKind::Email, &e.id)
    } else if let Some(w) = &a.webhook {
        (ActionKind::Webhook, &w.id)
    } else if let Some(w) = &a.slack_webhook {
        (ActionKind::SlackWebhook, &w.id)
    } else if let Some(w) = &a.mattermost_webhook {
        (ActionKind::MattermostWebhook, &w.id)
    } else if let Some(t) = &a.telegram {
        (ActionKind::Telegram, &t.id)
    } else {
        return Ok(None);
    };
    let id = id.as_ref().map(|i| parse_action_id(kind, i)).transpose()?;
    let old = id.and_then(|id| stored.iter().find(|s| s.id == id));
    let webhook = |enabled, include_results, url: &str, problems: &mut Vec<String>| {
        webhook_spec(
            kind,
            enabled,
            include_results,
            &keep_secret(url, kind, old),
            problems,
        )
    };
    let spec = if let Some(e) = &a.email {
        email_spec(ctx, actor, &e.update, old, problems).await?
    } else if let Some(u) = a.webhook.as_ref().map(|w| &w.update) {
        webhook(u.enabled, u.include_results, &u.url, problems)
    } else if let Some(u) = a.slack_webhook.as_ref().map(|w| &w.update) {
        webhook(u.enabled, u.include_results, &u.url, problems)
    } else if let Some(u) = a.mattermost_webhook.as_ref().map(|w| &w.update) {
        webhook(u.enabled, u.include_results, &u.url, problems)
    } else if let Some(t) = &a.telegram {
        telegram_spec(
            &MonitorTelegramInput {
                bot_token: keep_secret(&t.update.bot_token, kind, old),
                ..t.update.clone()
            },
            problems,
        )
    } else {
        return Ok(None);
    };
    Ok(Some(ActionEdit { id, spec }))
}

fn check_monitor(description: &str, query: &str, actions: usize, problems: &mut Vec<String>) {
    if description.trim().is_empty() {
        problems.push("The description is empty.".to_owned());
    } else if description.chars().count() > MAX_DESCRIPTION_LEN {
        problems.push(format!(
            "The description is longer than {MAX_DESCRIPTION_LEN} characters."
        ));
    }
    if query.len() > MAX_QUERY_LEN {
        problems.push(format!("The query is longer than {MAX_QUERY_LEN} bytes."));
    } else if let Err(e) = sourcegull_monitors::validate_query(query) {
        problems.push(e);
    }
    if actions > MAX_ACTIONS {
        problems.push(format!("A watch can have at most {MAX_ACTIONS} actions."));
    }
}

fn finish(problems: Vec<String>) -> Result<()> {
    if problems.is_empty() {
        Ok(())
    } else {
        Err(validation(problems))
    }
}

/// Fails unless `actor` may make a watch unfiltered.
fn require_may_share_unfiltered(ctx: &Context<'_>, actor: &Actor) -> Result<()> {
    sourcegull_monitors::access::require_may_share_unfiltered(
        actor,
        &services(ctx)?.config.load().value,
    )
    .map_err(access_error)
}

/// Parses an action ID of `kind`.
fn parse_action_id(kind: ActionKind, id: &ID) -> Result<i64> {
    unmarshal_int_id(action_kind_name(kind), id).ok_or_else(|| invalid_id("action", id))
}

/// The sender of test messages, with the web role's mailer when there is one.
fn sender(ctx: &Context<'_>) -> Result<ActionSender> {
    let s = services(ctx)?;
    let email: Arc<dyn sourcegull_services::email::EmailSender> = match &s.app {
        Some(app) => app.email_sender.clone(),
        None => Arc::new(sourcegull_services::email::SmtpEmailSender),
    };
    Ok(ActionSender::new(&s.db, s.config.clone(), email).with_keyring(s.keyring.clone()))
}

/// The stored action a test is sent for: one of `kind` on a watch the viewer may manage.
async fn tested_action(
    ctx: &Context<'_>,
    actor: &Actor,
    kind: ActionKind,
    id: Option<&ID>,
) -> Result<Option<Action>> {
    let Some(id) = id else {
        return Ok(None);
    };
    let a = store(ctx)?
        .action(parse_action_id(kind, id)?)
        .await
        .map_err(db_error)?
        .filter(|a| a.spec.kind == kind)
        .ok_or_else(|| access_error(sourcegull_monitors::access::not_found()))?;
    load_for_admin(ctx, actor, a.monitor_id).await?;
    Ok(Some(a))
}

async fn send_test(
    ctx: &Context<'_>,
    description: &str,
    spec: &ActionSpec,
) -> Result<EmptyResponse> {
    let sender = sender(ctx)?;
    let external_url = sender.external_url();
    let n = Notification {
        monitor_id: 0,
        description: description.trim(),
        query: "",
        results: &[],
        external_url: &external_url,
        test: true,
    };
    sender.send(spec, &n).await.map_err(|e| {
        error(
            codes::INVALID_INPUT,
            format!("The test message failed: {e}"),
        )
    })?;
    Ok(EmptyResponse)
}

/// Watch queries.
#[derive(Default)]
pub struct CodeMonitorsQuery;

#[Object]
impl CodeMonitorsQuery {
    /// Whether the viewer may make a watch send unfiltered results: site admins, and
    /// everyone when `watches.allowUnfilteredResults` is on.
    async fn viewer_can_share_unfiltered_watch_results(&self, ctx: &Context<'_>) -> Result<bool> {
        let Some(actor) = viewer(ctx) else {
            return Ok(false);
        };
        Ok(sourcegull_monitors::access::may_share_unfiltered(
            actor,
            &services(ctx)?.config.load().value,
        ))
    }

    /// Every watch (site admins only).
    async fn monitors(
        &self,
        ctx: &Context<'_>,
        first: i32,
        after: Option<String>,
    ) -> Result<MonitorConnection> {
        require_site_admin(ctx)?;
        list(ctx, None, first, after).await
    }
}

/// Watch mutations.
#[derive(Default)]
pub struct CodeMonitorsMutation;

#[Object]
impl CodeMonitorsMutation {
    /// Creates a watch.
    async fn create_code_monitor(
        &self,
        ctx: &Context<'_>,
        monitor: MonitorInput,
        trigger: MonitorTriggerInput,
        actions: Vec<MonitorActionInput>,
    ) -> Result<MonitorNode> {
        let actor = require_viewer(ctx)?;
        let ns = parse_namespace_id(&monitor.namespace)?;
        if load_namespace(ctx, ns).await?.is_none() {
            return Err(error(codes::NOT_FOUND, "Namespace not found."));
        }
        require_can_own(ctx, actor, ns).await?;
        if monitor.unfiltered_results {
            require_may_share_unfiltered(ctx, actor)?;
        }
        let mut problems = Vec::new();
        check_monitor(
            &monitor.description,
            &trigger.query,
            actions.len(),
            &mut problems,
        );
        let mut specs = Vec::with_capacity(actions.len());
        for a in &actions {
            specs.extend(action_spec(ctx, actor, a, &mut problems).await?);
        }
        finish(problems)?;
        let m = store(ctx)?
            .create(&NewMonitor {
                namespace: ns,
                description: monitor.description.trim().to_owned(),
                enabled: monitor.enabled,
                query: trigger.query.trim().to_owned(),
                actions: specs,
                created_by: actor.user_id,
                unfiltered: monitor.unfiltered_results,
            })
            .await
            .map_err(db_error)?;
        log_backend(
            ctx,
            sourcegull_analytics::names::MONITOR_CREATED,
            serde_json::json!({}),
        );
        Ok(MonitorNode(m))
    }

    /// Enables or disables a watch.
    async fn toggle_code_monitor(
        &self,
        ctx: &Context<'_>,
        id: ID,
        enabled: bool,
    ) -> Result<MonitorNode> {
        let actor = require_viewer(ctx)?;
        let m = load_for_admin(ctx, actor, parse_monitor_id(&id)?).await?;
        Ok(MonitorNode(
            store(ctx)?
                .set_enabled(m.id, enabled, actor.user_id)
                .await
                .map_err(db_error)?,
        ))
    }

    /// Deletes a watch with its actions and logs.
    async fn delete_code_monitor(&self, ctx: &Context<'_>, id: ID) -> Result<EmptyResponse> {
        let actor = require_viewer(ctx)?;
        let m = load_for_admin(ctx, actor, parse_monitor_id(&id)?).await?;
        store(ctx)?.delete(m.id).await.map_err(db_error)?;
        Ok(EmptyResponse)
    }

    /// Updates a watch: the request holds the complete watch; stored actions missing
    /// from it are deleted, and actions without an id are created.
    async fn update_code_monitor(
        &self,
        ctx: &Context<'_>,
        monitor: MonitorEditInput,
        trigger: MonitorEditTriggerInput,
        actions: Vec<MonitorEditActionInput>,
    ) -> Result<MonitorNode> {
        let actor = require_viewer(ctx)?;
        let m = load_for_admin(ctx, actor, parse_monitor_id(&monitor.id)?).await?;
        if unmarshal_int_id(TRIGGER_QUERY_KIND, &trigger.id) != Some(m.id) {
            return Err(error(
                codes::INVALID_INPUT,
                "The trigger does not belong to the watch.",
            ));
        }
        let ns = parse_namespace_id(&monitor.update.namespace)?;
        if load_namespace(ctx, ns).await?.is_none() {
            return Err(error(codes::NOT_FOUND, "Namespace not found."));
        }
        require_can_own(ctx, actor, ns).await?;
        if monitor.update.unfiltered_results {
            require_may_share_unfiltered(ctx, actor)?;
        }
        let mut problems = Vec::new();
        check_monitor(
            &monitor.update.description,
            &trigger.update.query,
            actions.len(),
            &mut problems,
        );
        let stored = store(ctx)?.actions(m.id).await.map_err(db_error)?;
        let mut edits = Vec::with_capacity(actions.len());
        for a in &actions {
            edits.extend(action_edit(ctx, actor, a, &stored, &mut problems).await?);
        }
        finish(problems)?;
        let updated = store(ctx)?
            .update(
                m.id,
                &MonitorUpdate {
                    namespace: ns,
                    description: monitor.update.description.trim().to_owned(),
                    enabled: monitor.update.enabled,
                    query: trigger.update.query.trim().to_owned(),
                    actions: edits,
                    updated_by: actor.user_id,
                    unfiltered: monitor.update.unfiltered_results,
                },
            )
            .await
            .map_err(db_error)?;
        Ok(MonitorNode(updated))
    }

    /// Makes the trigger's next run search the whole history of every watched revision and
    /// notify about everything it finds (site admins only).
    async fn reset_trigger_query_timestamps(
        &self,
        ctx: &Context<'_>,
        id: ID,
    ) -> Result<EmptyResponse> {
        require_site_admin(ctx)?;
        let monitor = unmarshal_int_id(TRIGGER_QUERY_KIND, &id)
            .ok_or_else(|| invalid_id("trigger query", &id))?;
        store(ctx)?.reset(monitor).await.map_err(db_error)?;
        Ok(EmptyResponse)
    }

    /// Sends a test email for a watch action.
    /// With `action`, a stored action's ID, an empty or masked header uses the stored one.
    async fn trigger_test_email_action(
        &self,
        ctx: &Context<'_>,
        namespace: ID,
        description: String,
        email: MonitorEmailInput,
        action: Option<ID>,
    ) -> Result<EmptyResponse> {
        let actor = require_viewer(ctx)?;
        require_can_own(ctx, actor, parse_namespace_id(&namespace)?).await?;
        let old = tested_action(ctx, actor, ActionKind::Email, action.as_ref()).await?;
        let mut problems = Vec::new();
        let spec = email_spec(ctx, actor, &email, old.as_ref(), &mut problems).await?;
        finish(problems)?;
        send_test(ctx, &description, &spec).await
    }

    /// Sends a test call for a watch's webhook action.
    /// With `action`, a stored action's ID, an empty or masked secret uses the stored one.
    async fn trigger_test_webhook_action(
        &self,
        ctx: &Context<'_>,
        namespace: ID,
        description: String,
        webhook: MonitorWebhookInput,
        action: Option<ID>,
    ) -> Result<EmptyResponse> {
        let actor = require_viewer(ctx)?;
        require_can_own(ctx, actor, parse_namespace_id(&namespace)?).await?;
        let kind = ActionKind::Webhook;
        let old = tested_action(ctx, actor, kind, action.as_ref()).await?;
        let mut problems = Vec::new();
        let spec = webhook_spec(
            kind,
            true,
            webhook.include_results,
            &keep_secret(&webhook.url, kind, old.as_ref()),
            &mut problems,
        );
        finish(problems)?;
        send_test(ctx, &description, &spec).await
    }

    /// Sends a test message for a watch's Slack webhook action.
    /// With `action`, a stored action's ID, an empty or masked secret uses the stored one.
    async fn trigger_test_slack_webhook_action(
        &self,
        ctx: &Context<'_>,
        namespace: ID,
        description: String,
        slack_webhook: MonitorSlackWebhookInput,
        action: Option<ID>,
    ) -> Result<EmptyResponse> {
        let actor = require_viewer(ctx)?;
        require_can_own(ctx, actor, parse_namespace_id(&namespace)?).await?;
        let kind = ActionKind::SlackWebhook;
        let old = tested_action(ctx, actor, kind, action.as_ref()).await?;
        let mut problems = Vec::new();
        let spec = webhook_spec(
            kind,
            true,
            slack_webhook.include_results,
            &keep_secret(&slack_webhook.url, kind, old.as_ref()),
            &mut problems,
        );
        finish(problems)?;
        send_test(ctx, &description, &spec).await
    }

    /// Sends a test message for a watch's Mattermost webhook action.
    /// With `action`, a stored action's ID, an empty or masked secret uses the stored one.
    async fn trigger_test_mattermost_webhook_action(
        &self,
        ctx: &Context<'_>,
        namespace: ID,
        description: String,
        mattermost_webhook: MonitorMattermostWebhookInput,
        action: Option<ID>,
    ) -> Result<EmptyResponse> {
        let actor = require_viewer(ctx)?;
        require_can_own(ctx, actor, parse_namespace_id(&namespace)?).await?;
        let kind = ActionKind::MattermostWebhook;
        let old = tested_action(ctx, actor, kind, action.as_ref()).await?;
        let mut problems = Vec::new();
        let spec = webhook_spec(
            kind,
            true,
            mattermost_webhook.include_results,
            &keep_secret(&mattermost_webhook.url, kind, old.as_ref()),
            &mut problems,
        );
        finish(problems)?;
        send_test(ctx, &description, &spec).await
    }

    /// Sends a test message for a watch's Telegram action.
    /// With `action`, a stored action's ID, an empty or masked secret uses the stored one.
    async fn trigger_test_telegram_action(
        &self,
        ctx: &Context<'_>,
        namespace: ID,
        description: String,
        telegram: MonitorTelegramInput,
        action: Option<ID>,
    ) -> Result<EmptyResponse> {
        let actor = require_viewer(ctx)?;
        require_can_own(ctx, actor, parse_namespace_id(&namespace)?).await?;
        let kind = ActionKind::Telegram;
        let old = tested_action(ctx, actor, kind, action.as_ref()).await?;
        let mut problems = Vec::new();
        let spec = telegram_spec(
            &MonitorTelegramInput {
                enabled: true,
                bot_token: keep_secret(&telegram.bot_token, kind, old.as_ref()),
                ..telegram
            },
            &mut problems,
        );
        finish(problems)?;
        send_test(ctx, &description, &spec).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_secrets() {
        let slack = "https://hooks.slack.com/services/T000/B000/XXXXa9f3";
        assert_eq!(mask_token("123456:ABC-def_123xyz"), "…3xyz");
        assert_eq!(mask_token("1:a"), "…");

        let stored = |kind, url: &str| Action {
            id: 1,
            monitor_id: 1,
            spec: ActionSpec {
                kind,
                enabled: true,
                include_results: false,
                url: url.to_owned(),
                chat_id: String::new(),
                priority: Priority::Normal,
                header: String::new(),
                recipients: Vec::new(),
            },
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let hook = stored(ActionKind::SlackWebhook, slack);
        let kind = ActionKind::SlackWebhook;
        assert_eq!(keep_secret("", kind, Some(&hook)), slack);
        assert_eq!(keep_secret(&mask_url(slack), kind, Some(&hook)), slack);
        assert_eq!(
            keep_secret(" https://x.example/y ", kind, Some(&hook)),
            "https://x.example/y"
        );
        assert_eq!(keep_secret("", kind, None), "");
        assert_eq!(keep_secret("", ActionKind::Webhook, Some(&hook)), "");

        let header = "Approved: list-password";
        assert_eq!(mask_header(header), "Approved: …word");
        assert_eq!(mask_header("Approved: short"), "Approved: …");
        assert_eq!(mask_header(""), "");
        let mut email = stored(ActionKind::Email, "");
        email.spec.header = header.to_owned();
        assert_eq!(keep_header("", false, Some(&email)), header);
        assert_eq!(keep_header("Approved: …word", false, Some(&email)), header);
        assert_eq!(keep_header(" X-A: b ", false, Some(&email)), "X-A: b");
        assert_eq!(keep_header("Approved: …word", true, Some(&email)), "");
        assert_eq!(keep_header("", false, None), "");
        assert_eq!(keep_header("", false, Some(&hook)), "");
    }

    #[test]
    fn telegram_inputs() {
        assert!(is_bot_token("123456:ABC-def_123"));
        for bad in [
            "", "123456", ":abc", "abc:def", "123:", "123:a b", "123:a/b",
        ] {
            assert!(!is_bot_token(bad), "{bad:?}");
        }
        for ok in ["12345", "-1001234567890", "@sourcegull_alerts"] {
            assert!(is_chat_id(ok), "{ok:?}");
        }
        for bad in ["", "-", "@", "@abc", "@a-b-c-d", "12a", "--1"] {
            assert!(!is_chat_id(bad), "{bad:?}");
        }
    }
}
