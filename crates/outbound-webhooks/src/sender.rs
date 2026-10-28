//! Delivery: a pgqueue worker over `outbound_webhook_jobs`.
//!
//! A job is one event or one channel message. A channel job (a watch action's rendered
//! message) is sent through its channel to its target ([`crate::channel`]); a failure fails
//! the job and it is retried like an event. An event job's handler sends the payload to every
//! webhook subscribed to the event type (and scope) at that moment, as `POST` with a JSON body
//! and the headers:
//!
//! - `Content-Type: application/json`
//! - [`EVENT_TYPE_HEADER`] (`X-Sourcegull-Webhook-Event-Type`): the event type, e.g.
//!   `changeset:publish`;
//! - [`SIGNATURE_HEADER`] (`X-Sourcegull-Webhook-Signature`): the lowercase hex HMAC-SHA256 of the
//!   exact body bytes, keyed with the webhook's secret ([`sign`]); receivers recompute it and
//!   compare in constant time;
//! - [`DELIVERY_HEADER`] (`X-Sourcegull-Webhook-Delivery`): the job id, the same for every attempt
//!   and webhook of one event, so receivers can drop duplicates.
//!
//! Each attempt is logged in `outbound_webhook_logs` (request, response with a truncated body,
//! or the error). A 2xx response delivers the event to that webhook; if any webhook failed
//! (other status, connection error, timeout, refused by the [SSRF guard](crate::guard)), the
//! job fails and pgqueue retries it with exponential backoff
//! ([`OutboundWebhookStore::queue_config`]); a retry only sends to the webhooks not yet
//! delivered to.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use crate::channel::{ChannelJobRef, ChannelMessage};
use crate::store::{
    ChannelJob, LoggedRequest, LoggedResponse, NewOutboundWebhookLog, OutboundWebhook,
    OutboundWebhookJob, OutboundWebhookStore,
};
use async_trait::async_trait;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use sourcegull_config::SiteConfigHandle;
use sourcegull_crypto::Keyring;
use sourcegull_db::Db;
use sourcegull_pgqueue::{
    Handler, JobContext, JobError, Queue, QueueConfig, Record, Resetter, Worker, WorkerOptions,
};
use sourcegull_types::metric_names;
use url::Url;

use crate::guard::{self, Allowlist, Lookup, SystemLookup};

/// The event type header.
pub const EVENT_TYPE_HEADER: &str = "X-Sourcegull-Webhook-Event-Type";
/// The signature header: hex HMAC-SHA256 of the body with the webhook's secret.
pub const SIGNATURE_HEADER: &str = "X-Sourcegull-Webhook-Signature";
/// The delivery (job) id header.
pub const DELIVERY_HEADER: &str = "X-Sourcegull-Webhook-Delivery";

/// Timeout of one delivery attempt (connect, redirects, response body).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// Response bytes kept in a delivery log.
pub const MAX_LOGGED_BODY: usize = 64 * 1024;

/// Counter of delivery attempts to registered webhooks, by `outcome` (`ok`, `error`).
pub const DELIVERIES_METRIC: &str = metric_names::OUTBOUND_WEBHOOK_DELIVERIES_TOTAL;

/// Counter of watch action deliveries, by `kind` (the channel name) and `outcome`; attempts of
/// channel jobs that belong to a watch action count here.
pub const WATCH_DELIVERIES_METRIC: &str = metric_names::CODE_MONITOR_DELIVERIES_TOTAL;

/// The lowercase hex HMAC-SHA256 of `body` keyed with `secret`.
pub fn sign(secret: &str, body: &[u8]) -> String {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(body);
    hex::encode(mac.finalize().into_bytes())
}

/// Sends queued events to their webhooks.
#[derive(Clone)]
pub struct Sender {
    pub(crate) db: Db,
    pub(crate) keyring: Option<Arc<Keyring>>,
    pub(crate) config: SiteConfigHandle,
    pub(crate) lookup: Arc<dyn Lookup>,
    pub(crate) timeout: Duration,
    /// The Telegram Bot API's base URL ([`crate::channel::TELEGRAM_API`]).
    pub(crate) telegram_api: String,
}

impl std::fmt::Debug for Sender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sender")
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

/// The outcome of one attempt.
pub(crate) struct Attempt {
    /// 0 when no response arrived.
    pub(crate) status: i32,
    pub(crate) response: Option<LoggedResponse>,
    pub(crate) error: Option<String>,
}

/// An error with its sources, e.g. `error sending request: … : 10.0.0.1 is not public …`.
pub(crate) fn error_chain(e: &dyn std::error::Error) -> String {
    let mut out = e.to_string();
    let mut source = e.source();
    while let Some(s) = source {
        let msg = s.to_string();
        if !out.contains(&msg) {
            out.push_str(": ");
            out.push_str(&msg);
        }
        source = s.source();
    }
    out
}

fn header_map(headers: &reqwest::header::HeaderMap) -> BTreeMap<String, Vec<String>> {
    let mut map: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (name, value) in headers {
        map.entry(name.as_str().to_owned())
            .or_default()
            .push(String::from_utf8_lossy(value.as_bytes()).into_owned());
    }
    map
}

impl Sender {
    /// `keyring` decrypts webhooks and payloads (without it every job fails); `config` gives
    /// the allowlist, read at each job.
    pub fn new(db: Db, keyring: Option<Arc<Keyring>>, config: SiteConfigHandle) -> Self {
        Self {
            db,
            keyring,
            config,
            lookup: Arc::new(SystemLookup),
            timeout: DEFAULT_TIMEOUT,
            telegram_api: crate::channel::TELEGRAM_API.to_owned(),
        }
    }

    /// Replaces the keyring (see [`Sender::new`]).
    #[must_use]
    pub fn with_keyring(mut self, keyring: Option<Arc<Keyring>>) -> Self {
        self.keyring = keyring;
        self
    }

    /// Replaces the Telegram Bot API's base URL (tests).
    #[must_use]
    pub fn with_telegram_api(mut self, url: &str) -> Self {
        self.telegram_api = url.trim_end_matches('/').to_owned();
        self
    }

    /// Replaces the timeout of one attempt.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Replaces the host name resolver (tests).
    #[must_use]
    pub fn with_lookup(mut self, lookup: Arc<dyn Lookup>) -> Self {
        self.lookup = lookup;
        self
    }

    pub(crate) fn store(&self) -> OutboundWebhookStore {
        OutboundWebhookStore::new(&self.db, self.keyring.clone())
    }

    /// Delivers job `job_id`: a channel job to its target, an event to the subscribed webhooks
    /// it was not delivered to yet. Fails with a retryable error saying what failed.
    pub async fn deliver(&self, job_id: i64, job: &OutboundWebhookJob) -> Result<(), JobError> {
        let store = self.store();
        let payload = store.payload(job).map_err(JobError::fatal)?;
        if let Some(channel) = &job.channel {
            return self
                .deliver_channel_job(&store, job_id, channel, &payload)
                .await;
        }
        let webhooks = store
            .subscribers(&job.event_type, job.scope.as_deref())
            .await
            .map_err(JobError::retry)?;
        let delivered = store.delivered(job_id).await.map_err(JobError::retry)?;
        let pending: Vec<&OutboundWebhook> = webhooks
            .iter()
            .filter(|w| !delivered.contains(&w.id))
            .collect();
        if pending.is_empty() {
            return Ok(());
        }
        let allow = Arc::new(Allowlist::new(
            &self.config.load().value.webhooks_outbound_allowed_hosts,
        ));
        let client = guard::client(allow.clone(), self.lookup.clone(), self.timeout)
            .map_err(JobError::retry)?;
        let mut failed = Vec::new();
        for webhook in pending {
            let (request, attempt) = self
                .attempt(&client, &allow, job_id, job, webhook, &payload)
                .await;
            let ok = attempt.ok();
            metrics::counter!(DELIVERIES_METRIC, "outcome" => if ok { "ok" } else { "error" })
                .increment(1);
            if !ok {
                tracing::info!(
                    job = job_id,
                    webhook = webhook.id,
                    status = attempt.status,
                    error = attempt.error.as_deref().unwrap_or(""),
                    "outbound webhook delivery failed"
                );
                failed.push(format!(
                    "webhook {}: {}",
                    webhook.id,
                    attempt
                        .error
                        .clone()
                        .unwrap_or_else(|| format!("status {}", attempt.status))
                ));
            }
            store
                .create_log(&NewOutboundWebhookLog {
                    job_id,
                    outbound_webhook_id: webhook.id,
                    status_code: attempt.status,
                    request,
                    response: attempt.response,
                    error: attempt.error,
                })
                .await
                .map_err(JobError::retry)?;
        }
        if failed.is_empty() {
            Ok(())
        } else {
            Err(JobError::retry(failed.join("; ")))
        }
    }

    async fn deliver_channel_job(
        &self,
        store: &OutboundWebhookStore,
        job_id: i64,
        job: &ChannelJob,
        payload: &[u8],
    ) -> Result<(), JobError> {
        let (target, secret) = store.channel_target(job).map_err(JobError::fatal)?;
        let body: serde_json::Value = serde_json::from_slice(payload).map_err(JobError::fatal)?;
        let r = self
            .deliver_channel(&ChannelMessage {
                channel: job.channel,
                target: &target,
                body: &body,
                secret: secret.as_deref(),
                job: Some(ChannelJobRef::Outbound(job_id)),
            })
            .await;
        if job.cm_action_id.is_some() {
            metrics::counter!(
                WATCH_DELIVERIES_METRIC,
                "kind" => job.channel.as_str(),
                "outcome" => if r.is_ok() { "ok" } else { "error" }
            )
            .increment(1);
        }
        r.map_err(JobError::retry)
    }

    async fn attempt(
        &self,
        client: &reqwest::Client,
        allow: &Allowlist,
        job_id: i64,
        job: &OutboundWebhookJob,
        webhook: &OutboundWebhook,
        payload: &[u8],
    ) -> (LoggedRequest, Attempt) {
        let headers = [
            ("content-type", "application/json".to_owned()),
            (EVENT_TYPE_HEADER, job.event_type.clone()),
            (SIGNATURE_HEADER, sign(&webhook.secret, payload)),
            (DELIVERY_HEADER, job_id.to_string()),
        ];
        let request = LoggedRequest {
            method: "POST".into(),
            url: webhook.url.clone(),
            version: "HTTP/1.1".into(),
            headers: headers
                .iter()
                .map(|(k, v)| (k.to_ascii_lowercase(), vec![v.clone()]))
                .collect(),
            body: String::from_utf8_lossy(payload).into_owned(),
        };
        let url = match Url::parse(&webhook.url) {
            Ok(u) => u,
            Err(e) => return (request, Attempt::failed(format!("invalid URL: {e}"))),
        };
        if let Err(e) = guard::check_url(&url, allow) {
            return (request, Attempt::failed(e.to_string()));
        }
        let attempt = post(client, url, &headers, payload, false).await;
        (request, attempt)
    }
}

impl Attempt {
    pub(crate) fn failed(error: String) -> Self {
        Attempt {
            status: 0,
            response: None,
            error: Some(error),
        }
    }

    pub(crate) fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// `POST`s `payload` with `headers` to `url` (checked by [`guard::check_url`]) through the
/// guarded `client` and reads the response. With `hide_url`, request errors do not quote the
/// URL (it may hold a credential).
pub(crate) async fn post(
    client: &reqwest::Client,
    url: Url,
    headers: &[(&str, String)],
    payload: &[u8],
    hide_url: bool,
) -> Attempt {
    let mut builder = client.post(url).body(payload.to_vec());
    for (name, value) in headers {
        builder = builder.header(*name, value);
    }
    let resp = match builder.send().await {
        Ok(r) => r,
        Err(e) if hide_url => return Attempt::failed(error_chain(&e.without_url())),
        Err(e) => return Attempt::failed(error_chain(&e)),
    };
    let status = i32::from(resp.status().as_u16());
    let mut response = LoggedResponse {
        status_code: status,
        version: format!("{:?}", resp.version()),
        headers: header_map(resp.headers()),
        body: String::new(),
    };
    let mut error = None;
    match read_limited(resp, MAX_LOGGED_BODY).await {
        Ok(body) => response.body = body,
        Err(e) if hide_url => {
            error = Some(format!(
                "reading the response: {}",
                error_chain(&e.without_url())
            ));
        }
        Err(e) => error = Some(format!("reading the response: {}", error_chain(&e))),
    }
    if error.is_none() && !(200..300).contains(&status) {
        error = Some(format!("unexpected status {status}"));
    }
    Attempt {
        status,
        response: Some(response),
        error,
    }
}

/// Reads at most `limit` bytes of the body (lossy UTF-8).
async fn read_limited(mut resp: reqwest::Response, limit: usize) -> reqwest::Result<String> {
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        let room = limit - body.len();
        body.extend_from_slice(&chunk[..chunk.len().min(room)]);
        if body.len() >= limit {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&body).into_owned())
}

/// Runs [`Sender::deliver`] on dequeued jobs.
pub struct SendHandler {
    sender: Sender,
}

impl SendHandler {
    pub fn new(sender: Sender) -> Self {
        Self { sender }
    }
}

#[async_trait]
impl Handler<OutboundWebhookJob> for SendHandler {
    async fn handle(
        &self,
        _ctx: JobContext,
        record: Record<OutboundWebhookJob>,
    ) -> Result<(), JobError> {
        self.sender.deliver(record.id, &record.payload).await
    }
}

/// Default concurrency of the sender worker.
pub const DEFAULT_CONCURRENCY: usize = 5;

/// The sender worker over `queue_config` (normally [`OutboundWebhookStore::queue_config`])
/// and its stalled-job resetter, ready to run.
pub fn sender_workers(
    sender: Sender,
    queue_config: QueueConfig,
    worker_id: &str,
) -> Result<(Worker<OutboundWebhookJob>, Resetter<OutboundWebhookJob>), sourcegull_pgqueue::Error> {
    let queue: Queue<OutboundWebhookJob> = Queue::new(sender.db.pool().clone(), queue_config)?;
    let mut opts = WorkerOptions::new(worker_id);
    opts.concurrency = DEFAULT_CONCURRENCY;
    let worker = Worker::new(queue.clone(), SendHandler::new(sender), opts);
    let resetter = Resetter::new(queue, Duration::from_secs(30));
    Ok((worker, resetter))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signatures() {
        // RFC 4231 test case 2.
        assert_eq!(
            sign("Jefe", b"what do ya want for nothing?"),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        assert_ne!(sign("a", b"x"), sign("b", b"x"));
    }
}
