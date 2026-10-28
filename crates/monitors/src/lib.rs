//! Watches: a `type:commit` or `type:diff` query owned by a user or
//! an org, evaluated with the owner's permissions on the commits each push adds, and actions
//! (email, generic webhook, Slack-compatible webhook, Mattermost webhook, Telegram bot) that
//! deliver what it finds.
//!
//! - [`access`]: who may see and manage watches, and send them email.
//! - [`store`]: the `cm_*` tables (watches, actions, recipients, last searched commits, and
//!   the trigger and action job queues that are the run and action logs).
//! - [`index`]: query validation and compilation, the repository → watches index and the
//!   pure range planner.
//! - [`evaluator`]: the leader-elected loop that follows `refs_updated` and sweeps every
//!   minute, queueing trigger jobs for the commit ranges that moved.
//! - [`trigger`]: the trigger job worker that searches the ranges as the owner and queues the
//!   actions.
//! - [`actions`] and [`render`]: the action job worker with retries, email over SMTP, and the
//!   email, webhook, Slack, Mattermost and Telegram payloads; the HTTP actions are queued and
//!   delivered as channel jobs of `sourcegull-outbound-webhooks` (its queue, retries, SSRF guard
//!   and delivery log).
//!
//! The GraphQL API is in `sourcegull-graphql`. Evaluation searches only the commit ranges that
//! changed, so the work follows the push rate rather than the number of watches times
//! repositories.

pub mod access;
pub mod actions;
pub mod evaluator;
mod feature;
pub mod index;
pub mod render;
pub mod results;
pub mod store;
pub mod trigger;

pub use actions::{ActionSender, action_workers, channel_message, parse_email_header};
pub use evaluator::{EVALUATOR, Evaluator, EvaluatorConfig, run_evaluator};
pub use feature::WatchesFeature;
pub use index::{MAX_RESULTS, TYPE_REQUIRED, validate_query};
pub use render::{Audience, MONITOR_KIND, Notification};
pub use results::MonitorResult;
pub use store::{
    Action, ActionEdit, ActionEvent, ActionKind, ActionSpec, ChannelAction, CommitRange,
    ListMonitors, Monitor, MonitorStore, MonitorUpdate, NewMonitor, Priority, TriggerEvent,
};
pub use trigger::{TriggerHandler, trigger_workers};
