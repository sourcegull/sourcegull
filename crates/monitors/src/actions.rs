//! Delivering notifications: rendering the HTTP actions' messages, the action job worker over
//! `cm_action_jobs` and the direct sends of "send test" (`triggerTest…Action`).
//!
//! - Email goes through the site's SMTP settings (`email.smtp`, the same [`EmailSender`] as
//!   password resets) to each recipient's verified primary address (every member's, for an
//!   org). The action's `header` (`Name: value`, see [`parse_email_header`]) is added to the
//!   message: it is the way to have a read-only or moderated mailing list approve the
//!   message (Mailman's `Approved: <password>`). When a recipient is an org, each user gets
//!   their own message with only the results in repositories they can read (none when there
//!   are none left), noted at the end, unless the watch is unfiltered
//!   ([`crate::access::unfiltered_owner`]): then everyone gets every result, with a warning.
//! - Webhooks, Slack and Mattermost webhooks, and Telegram messages (the action's bot calling
//!   the Bot API's `sendMessage`) are rendered here ([`channel_message`]) and delivered as
//!   channels of the outbound webhooks ([`sourcegull_outbound_webhooks::channel`]): through the
//!   SSRF guard, so non-public addresses are refused unless `webhooks.outbound.allowedHosts`
//!   allows them (any user can create a watch), with errors that never carry the URL (it may
//!   hold a credential), and with each attempt in the outbound webhook delivery log. When the
//!   site has encryption keys, the trigger job queues them as channel jobs on the outbound
//!   webhook queue ([`crate::trigger`]), whose sender delivers and retries them; otherwise
//!   they are action jobs like email.
//!
//! A failed delivery of an action job (SMTP error, non-2xx answer, connection error) fails the
//! job and pgqueue retries it with exponential backoff ([`MonitorStore::action_queue_config`]).
//! The states and failure messages of the action jobs and channel jobs are the action log
//! ([`MonitorStore::action_events`]).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use sourcegull_config::SiteConfigHandle;
use sourcegull_crypto::Keyring;
use sourcegull_db::{Db, Namespace};
use sourcegull_outbound_webhooks::guard::Lookup;
use sourcegull_outbound_webhooks::{Channel, ChannelJobRef, ChannelMessage};
use sourcegull_pgqueue::{
    Handler, JobContext, JobError, Queue, Record, Resetter, Worker, WorkerOptions,
};
use sourcegull_services::email::{EmailMessage, EmailSender};
use sourcegull_types::{RepoId, UserId, metric_names};

use crate::render::{self, Audience, Notification};
use crate::results::MonitorResult;
use crate::store::{ActionJob, ActionKind, ActionSpec, MonitorStore};

/// Counter of deliveries, by `kind` (`email`, `webhook`, `slack_webhook`,
/// `mattermost_webhook`, `telegram`) and `outcome`.
pub const DELIVERIES_METRIC: &str = metric_names::CODE_MONITOR_DELIVERIES_TOTAL;

/// The channel and rendered body of an HTTP action's message; `None` for email.
pub fn channel_message(action: &ActionSpec, n: &Notification<'_>) -> Option<(Channel, Value)> {
    let include = action.include_results;
    Some(match action.kind {
        ActionKind::Email => return None,
        ActionKind::Webhook => (Channel::Webhook, render::webhook_payload(n, include)),
        ActionKind::SlackWebhook => (Channel::SlackWebhook, render::slack_payload(n, include)),
        ActionKind::MattermostWebhook => (
            Channel::MattermostWebhook,
            render::mattermost_payload(n, include),
        ),
        ActionKind::Telegram => (
            Channel::Telegram,
            render::telegram_payload(n, &action.chat_id, include),
        ),
    })
}

/// Sends notifications through actions.
#[derive(Clone)]
pub struct ActionSender {
    db: Db,
    authz: sourcegull_authz::Checker,
    store: MonitorStore,
    config: SiteConfigHandle,
    email: Arc<dyn EmailSender>,
    channels: sourcegull_outbound_webhooks::Sender,
}

impl std::fmt::Debug for ActionSender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActionSender").finish_non_exhaustive()
    }
}

impl ActionSender {
    pub fn new(db: &Db, config: SiteConfigHandle, email: Arc<dyn EmailSender>) -> Self {
        Self {
            db: db.clone(),
            authz: sourcegull_authz::Checker::new(db.clone(), config.clone()),
            store: MonitorStore::new(db),
            channels: sourcegull_outbound_webhooks::Sender::new(db.clone(), None, config.clone()),
            config,
            email,
        }
    }

    /// Sets the keyring that decrypts webhook URLs (see [`MonitorStore::with_keyring`]) and
    /// encrypts delivery logs.
    #[must_use]
    pub fn with_keyring(mut self, keyring: Option<Arc<Keyring>>) -> Self {
        self.store = self.store.with_keyring(keyring.clone());
        self.channels = self.channels.with_keyring(keyring);
        self
    }

    /// Replaces the host name resolver (tests).
    #[must_use]
    pub fn with_lookup(mut self, lookup: Arc<dyn Lookup>) -> Self {
        self.channels = self.channels.with_lookup(lookup);
        self
    }

    /// Replaces the Telegram Bot API's base URL (tests).
    #[must_use]
    pub fn with_telegram_api(mut self, url: &str) -> Self {
        self.channels = self.channels.with_telegram_api(url);
        self
    }

    /// The site's external URL for links.
    pub fn external_url(&self) -> String {
        self.config
            .load()
            .value
            .external_url
            .clone()
            .unwrap_or_default()
    }

    /// Sends `n` through `action` (a test send: not in the delivery log).
    pub async fn send(&self, action: &ActionSpec, n: &Notification<'_>) -> Result<(), String> {
        self.send_for_job(action, n, None, None).await
    }

    /// Sends `n` through `action` for action job `job` (`None`: a test send). `shared_by`
    /// names the user whose access an unfiltered watch shares; `None` filters the results of
    /// each email recipient.
    async fn send_for_job(
        &self,
        action: &ActionSpec,
        n: &Notification<'_>,
        job: Option<i64>,
        shared_by: Option<&str>,
    ) -> Result<(), String> {
        let r = match channel_message(action, n) {
            None => self.send_email(action, n, shared_by).await,
            Some((channel, body)) => {
                self.channels
                    .deliver_channel(&ChannelMessage {
                        channel,
                        target: &action.url,
                        body: &body,
                        secret: None,
                        job: job.map(ChannelJobRef::WatchAction),
                    })
                    .await
            }
        };
        metrics::counter!(
            DELIVERIES_METRIC,
            "kind" => action.kind.as_str(),
            "outcome" => if r.is_ok() { "ok" } else { "error" }
        )
        .increment(1);
        r
    }

    async fn send_email(
        &self,
        action: &ActionSpec,
        n: &Notification<'_>,
        shared_by: Option<&str>,
    ) -> Result<(), String> {
        let mut to: Vec<(UserId, String)> = Vec::new();
        for r in &action.recipients {
            for (user, email) in self
                .store
                .recipient_users(*r)
                .await
                .map_err(|e| e.to_string())?
            {
                if !to.iter().any(|(u, _)| *u == user) {
                    to.push((user, email));
                }
            }
        }
        if to.is_empty() {
            return Err("No recipient has a verified email address.".to_owned());
        }
        let headers: Vec<(String, String)> =
            parse_email_header(&action.header)?.into_iter().collect();
        let to_org = action
            .recipients
            .iter()
            .any(|r| matches!(r, Namespace::Org(_)));
        let audience = match shared_by {
            _ if n.test || !to_org => Audience::Owner,
            Some(shared_by) => Audience::Everyone { shared_by },
            None => Audience::Recipient,
        };
        let shared = render::email(n, action.priority, action.include_results, audience);
        let config = self.config.load();
        for (user, address) in to {
            let mail = if audience == Audience::Recipient {
                let results = self.readable_results(user, n.results).await?;
                if results.is_empty() {
                    continue;
                }
                let mine = Notification {
                    results: &results,
                    ..n.clone()
                };
                render::email(&mine, action.priority, action.include_results, audience)
            } else {
                shared.clone()
            };
            self.email
                .send(
                    &config.value,
                    EmailMessage {
                        to: address.clone(),
                        subject: mail.subject,
                        text: mail.text,
                        html: mail.html,
                        headers: headers.clone(),
                    },
                )
                .await
                .map_err(|e| format!("sending to {address}: {e}"))?;
        }
        Ok(())
    }

    /// The results in repositories `user` can read (none once the user is gone).
    async fn readable_results(
        &self,
        user: UserId,
        results: &[MonitorResult],
    ) -> Result<Vec<MonitorResult>, String> {
        let Some(actor) = crate::trigger::user_actor(&self.db, user)
            .await
            .map_err(|e| e.to_string())?
        else {
            return Ok(Vec::new());
        };
        let access = self
            .authz
            .access(Some(&actor))
            .await
            .map_err(|e| e.to_string())?;
        Ok(results
            .iter()
            .filter(|r| access.can_read(RepoId(r.repository_id)))
            .cloned()
            .collect())
    }
}

/// Header fields an email action's `header` may not set: the ones the sender writes itself
/// and the ones that would change who receives the message or how it is parsed.
const RESERVED_HEADERS: &[&str] = &[
    "bcc",
    "cc",
    "date",
    "from",
    "message-id",
    "mime-version",
    "received",
    "reply-to",
    "resent-bcc",
    "resent-cc",
    "resent-from",
    "resent-to",
    "return-path",
    "sender",
    "subject",
    "to",
];

/// Parses an email action's `header`: empty (no extra header) or one `Name: value` field,
/// e.g. `Approved: list-password`. The name is printable ASCII without `:`, not a reserved
/// field (addressing, subject, `Content-*`, ...); the value is non-empty and on one line.
pub fn parse_email_header(header: &str) -> Result<Option<(String, String)>, String> {
    let header = header.trim();
    if header.is_empty() {
        return Ok(None);
    }
    let invalid = || {
        format!(
            "The email header {header:?} is not a single \"Name: value\" header field \
             (for example \"Approved: password\")."
        )
    };
    let (name, value) = header.split_once(':').ok_or_else(invalid)?;
    let (name, value) = (name.trim(), value.trim());
    if name.is_empty()
        || name.len() > 76
        || !name.bytes().all(|b| b.is_ascii_graphic() && b != b':')
        || value.is_empty()
        || value.chars().any(char::is_control)
    {
        return Err(invalid());
    }
    let lower = name.to_ascii_lowercase();
    if RESERVED_HEADERS.contains(&lower.as_str()) || lower.starts_with("content-") {
        return Err(format!(
            "The email header {name:?} is set by Sourcegull and cannot be overridden."
        ));
    }
    Ok(Some((name.to_owned(), value.to_owned())))
}

/// Runs action jobs: renders the trigger job's results and sends them. HTTP actions queue
/// channel jobs instead when the site has encryption keys; this worker still delivers their
/// action jobs queued without keys or before channel jobs existed.
pub struct ActionHandler {
    sender: ActionSender,
}

impl ActionHandler {
    pub fn new(sender: ActionSender) -> Self {
        Self { sender }
    }

    /// Delivers one action job.
    pub async fn deliver(&self, job_id: i64, job: &ActionJob) -> Result<(), JobError> {
        let store = &self.sender.store;
        let Some(action) = store.action(job.action_id).await.map_err(JobError::retry)? else {
            return Ok(());
        };
        let Some(event) = store
            .trigger_event(job.trigger_job_id)
            .await
            .map_err(JobError::retry)?
        else {
            return Ok(());
        };
        let Some(monitor) = store
            .get(action.monitor_id)
            .await
            .map_err(JobError::retry)?
        else {
            return Ok(());
        };
        // Turned off since the job was queued.
        if !monitor.enabled || !action.spec.enabled {
            return Ok(());
        }
        let results = event.results.unwrap_or_default();
        let shared = {
            let config = self.sender.config.load();
            crate::access::unfiltered_owner(&self.sender.db, &config.value, &monitor)
                .await
                .map_err(JobError::retry)?
        };
        let external_url = self.sender.external_url();
        let n = Notification {
            monitor_id: monitor.id,
            description: &monitor.description,
            query: &event.query,
            results: &results,
            external_url: &external_url,
            test: false,
        };
        self.sender
            .send_for_job(
                &action.spec,
                &n,
                Some(job_id),
                shared.as_ref().map(|a| a.username.as_str()),
            )
            .await
            .map_err(JobError::retry)
    }
}

#[async_trait]
impl Handler<ActionJob> for ActionHandler {
    async fn handle(&self, _ctx: JobContext, record: Record<ActionJob>) -> Result<(), JobError> {
        self.deliver(record.id, &record.payload).await
    }
}

/// The action job worker and its stalled-job resetter, ready to run.
pub fn action_workers(
    db: &Db,
    sender: ActionSender,
    worker_id: &str,
) -> Result<(Worker<ActionJob>, Resetter<ActionJob>), sourcegull_pgqueue::Error> {
    let queue: Queue<ActionJob> =
        Queue::new(db.pool().clone(), MonitorStore::action_queue_config())?;
    let mut opts = WorkerOptions::new(worker_id);
    opts.concurrency = 4;
    let worker = Worker::new(queue.clone(), ActionHandler::new(sender), opts);
    let resetter = Resetter::new(queue, Duration::from_secs(30));
    Ok((worker, resetter))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_email_headers() {
        assert_eq!(parse_email_header(""), Ok(None));
        assert_eq!(parse_email_header("  "), Ok(None));
        assert_eq!(
            parse_email_header(" Approved:  list-password "),
            Ok(Some(("Approved".into(), "list-password".into())))
        );
        assert_eq!(
            parse_email_header("X-Sourcegull-Monitor: a: b"),
            Ok(Some(("X-Sourcegull-Monitor".into(), "a: b".into())))
        );
        for bad in [
            "no colon",
            ": value",
            "Approved:",
            "Two words: x",
            "Approved: x\r\nBcc: eve@example.com",
            "Approved: x\nBcc: eve@example.com",
            "Bcc: eve@example.com",
            "subject: hi",
            "Content-Type: text/plain",
        ] {
            assert!(parse_email_header(bad).is_err(), "{bad:?}");
        }
    }
}
