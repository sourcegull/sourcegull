//! Channels: deliveries to one target given with the message rather than to the webhooks
//! subscribed to an event. Watch actions that post to a URL use them; the watch renders the
//! body and the channel gives the request its shape:
//!
//! - [`Channel::Webhook`]: `POST` of the JSON body to the URL, with [`DELIVERY_HEADER`] (the
//!   id of the job the message belongs to, the same for every attempt) and, when the message
//!   has a secret,
//!   [`SIGNATURE_HEADER`] as for registered webhooks;
//! - [`Channel::SlackWebhook`] and [`Channel::MattermostWebhook`]: `POST` of the JSON body to
//!   the incoming webhook URL;
//! - [`Channel::Telegram`]: the Bot API's `sendMessage` ([`TELEGRAM_API`]) with the target as
//!   the bot token.
//!
//! Every request goes through the [SSRF guard](crate::guard) with the site's
//! `webhooks.outbound.allowedHosts`. Errors never quote the URL, which may hold a credential (a
//! webhook token, a bot token). An attempt that belongs to a job is recorded in
//! `outbound_webhook_logs` ([`crate::store::NewChannelLog`]) with the URL reduced to its origin;
//! test sends are not logged, nor is anything when no keyring is configured.
//!
//! Watches queue their messages as channel jobs on `outbound_webhook_jobs`
//! ([`crate::store::OutboundWebhookStore::enqueue_channel`]); the sender worker delivers them
//! with [`Sender::deliver_channel`] and retries them like events. A failed delivery returns
//! the error, so a caller with its own queue (the watches' action jobs) retries its job.

use std::sync::Arc;

use crate::store::{LoggedRequest, NewChannelLog};
use serde_json::Value;
use url::Url;

use crate::guard::{self, Allowlist};
use crate::sender::{Attempt, DELIVERY_HEADER, SIGNATURE_HEADER, Sender, error_chain, post, sign};

/// The Telegram Bot API.
pub const TELEGRAM_API: &str = "https://api.telegram.org";

/// How a message is sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Channel {
    Webhook,
    SlackWebhook,
    MattermostWebhook,
    Telegram,
}

impl Channel {
    /// Every channel.
    pub const ALL: [Channel; 4] = [
        Channel::Webhook,
        Channel::SlackWebhook,
        Channel::MattermostWebhook,
        Channel::Telegram,
    ];

    /// The channel named `name` ([`Channel::as_str`]).
    pub fn parse(name: &str) -> Option<Channel> {
        Channel::ALL.into_iter().find(|c| c.as_str() == name)
    }

    /// The name in delivery logs and queued jobs.
    pub const fn as_str(self) -> &'static str {
        match self {
            Channel::Webhook => "webhook",
            Channel::SlackWebhook => "slack_webhook",
            Channel::MattermostWebhook => "mattermost_webhook",
            Channel::Telegram => "telegram",
        }
    }
}

/// One message to deliver through a channel.
#[derive(Debug, Clone, Copy)]
pub struct ChannelMessage<'a> {
    pub channel: Channel,
    /// The URL; for [`Channel::Telegram`], the bot token.
    pub target: &'a str,
    /// The rendered JSON body.
    pub body: &'a Value,
    /// [`Channel::Webhook`]: signs the body with this key.
    pub secret: Option<&'a str>,
    /// The job the delivery belongs to; `None` (a test send) is not logged.
    pub job: Option<ChannelJobRef>,
}

/// The job a channel delivery belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelJobRef {
    /// A channel job of `outbound_webhook_jobs`.
    Outbound(i64),
    /// A watch action job of `cm_action_jobs`.
    WatchAction(i64),
}

impl ChannelJobRef {
    fn id(self) -> i64 {
        match self {
            ChannelJobRef::Outbound(id) | ChannelJobRef::WatchAction(id) => id,
        }
    }
}

/// The URL as logged: its origin, without the path and query that may hold a credential.
fn masked(url: &str) -> String {
    match Url::parse(url) {
        Ok(u) => format!("{}/…", u.origin().ascii_serialization()),
        Err(_) => String::new(),
    }
}

/// The first `max` characters of `s`.
fn truncate(s: &str, max: usize) -> &str {
    s.char_indices().nth(max).map_or(s, |(i, _)| &s[..i])
}

impl Sender {
    /// Delivers `msg` once. Returns the error of a refused, failed or non-2xx attempt; a JSON
    /// `description` in the answer (the Bot API's and some webhooks' explanation) is added
    /// to it.
    pub async fn deliver_channel(&self, msg: &ChannelMessage<'_>) -> Result<(), String> {
        let url = match msg.channel {
            Channel::Telegram => format!("{}/bot{}/sendMessage", self.telegram_api, msg.target),
            _ => msg.target.to_owned(),
        };
        let payload = serde_json::to_vec(msg.body).map_err(|e| e.to_string())?;
        let mut headers = vec![("content-type", "application/json".to_owned())];
        if msg.channel == Channel::Webhook {
            if let Some(job) = msg.job {
                headers.push((DELIVERY_HEADER, job.id().to_string()));
            }
            if let Some(secret) = msg.secret {
                headers.push((SIGNATURE_HEADER, sign(secret, &payload)));
            }
        }
        let request = LoggedRequest {
            method: "POST".into(),
            url: masked(&url),
            version: "HTTP/1.1".into(),
            headers: headers
                .iter()
                .map(|(k, v)| (k.to_ascii_lowercase(), vec![v.clone()]))
                .collect(),
            body: String::from_utf8_lossy(&payload).into_owned(),
        };
        let attempt = self.attempt_channel(&url, &headers, &payload).await;
        let result = if attempt.ok() {
            Ok(())
        } else {
            let detail = attempt
                .response
                .as_ref()
                .and_then(|r| serde_json::from_str::<Value>(&r.body).ok())
                .and_then(|v| v.get("description")?.as_str().map(str::to_owned));
            Err(match (attempt.error.clone(), detail) {
                (_, Some(d)) => format!(
                    "unexpected status {}: {}",
                    attempt.status,
                    truncate(&d, 200)
                ),
                (Some(e), None) => e,
                (None, None) => format!("unexpected status {}", attempt.status),
            })
        };
        if let Some(job) = msg.job {
            self.log_channel(job, msg.channel, request, attempt, result.as_ref().err())
                .await;
        }
        result
    }

    async fn attempt_channel(
        &self,
        url: &str,
        headers: &[(&str, String)],
        payload: &[u8],
    ) -> Attempt {
        let url = match Url::parse(url) {
            Ok(u) => u,
            Err(e) => return Attempt::failed(format!("invalid URL: {e}")),
        };
        let allow = Arc::new(Allowlist::new(
            &self.config.load().value.webhooks_outbound_allowed_hosts,
        ));
        if let Err(e) = guard::check_url(&url, &allow) {
            return Attempt::failed(e.to_string());
        }
        let client = match guard::client(allow, self.lookup.clone(), self.timeout) {
            Ok(c) => c,
            Err(e) => return Attempt::failed(error_chain(&e.without_url())),
        };
        post(&client, url, headers, payload, true).await
    }

    async fn log_channel(
        &self,
        job: ChannelJobRef,
        channel: Channel,
        request: LoggedRequest,
        attempt: Attempt,
        error: Option<&String>,
    ) {
        if self.keyring.is_none() {
            return;
        }
        let (job_id, cm_action_job_id) = match job {
            ChannelJobRef::Outbound(id) => (Some(id), None),
            ChannelJobRef::WatchAction(id) => (None, Some(id)),
        };
        let log = NewChannelLog {
            job_id,
            cm_action_job_id,
            channel: channel.as_str().to_owned(),
            status_code: attempt.status,
            request,
            response: attempt.response,
            error: error.cloned(),
        };
        if let Err(e) = self.store().create_channel_log(&log).await {
            tracing::warn!(job = job.id(), error = %e, "recording a channel delivery failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logged_urls_keep_only_the_origin() {
        assert_eq!(
            masked("https://hooks.slack.com/services/T0/B0/secret?x=1"),
            "https://hooks.slack.com/…"
        );
        assert_eq!(
            masked("http://127.0.0.1:8080/bot123:abc/sendMessage"),
            "http://127.0.0.1:8080/…"
        );
        assert_eq!(masked("not a url"), "");
    }

    #[test]
    fn channel_names_round_trip() {
        for c in Channel::ALL {
            assert_eq!(Channel::parse(c.as_str()), Some(c));
        }
        assert_eq!(Channel::parse("email"), None);
    }
}
