//! `outbound_webhooks`, `outbound_webhook_event_types`, `outbound_webhook_jobs` and
//! `outbound_webhook_logs`.
//!
//! A webhook is a URL and an HMAC secret subscribed to event types, each optionally limited to
//! a scope. An event is one row of the `outbound_webhook_jobs` queue; its worker sends the
//! payload to every subscribed webhook and records each attempt in `outbound_webhook_logs`.
//! The same queue holds channel jobs: one rendered message of a watch action for one channel
//! and target ([`NewChannelJob`]), whose attempts are logged by job without a webhook. The log
//! also holds the attempts of watch action jobs delivered from the watches' own queue, keyed
//! by their `cm_action_jobs` row ([`NewChannelLog`]). URLs, targets, secrets, payloads and logs
//! are encrypted with the `outbound_webhook` data key, so writing or reading them needs the
//! keyring; without one those calls fail with [`DbError::Invalid`]. Who may manage webhooks
//! (site admins) is the API's concern.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sourcegull_crypto::{Keyring, RecordClass};
use sourcegull_db::{Db, DbError};
use sourcegull_pgqueue::{QueueConfig, Record, notify_enqueued};
use sourcegull_types::UserId;
use sqlx::postgres::PgRow;
use sqlx::{PgConnection, PgPool, Row};

use crate::channel::Channel;

type Result<T> = std::result::Result<T, DbError>;

/// The queue table of outbound webhook events.
pub const OUTBOUND_WEBHOOK_JOBS_TABLE: &str = "outbound_webhook_jobs";

/// An event type a webhook subscribes to, optionally limited to one scope.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ScopedEventType {
    pub event_type: String,
    /// `None`: events of every scope.
    pub scope: Option<String>,
}

/// A webhook with its decrypted URL and secret.
#[derive(Clone, PartialEq, Eq)]
pub struct OutboundWebhook {
    pub id: i64,
    pub url: String,
    /// The HMAC-SHA256 key payloads are signed with.
    pub secret: String,
    /// Sorted by event type and scope.
    pub event_types: Vec<ScopedEventType>,
    /// `None` once the user has been deleted.
    pub created_by: Option<UserId>,
    pub created_at: DateTime<Utc>,
    pub updated_by: Option<UserId>,
    pub updated_at: DateTime<Utc>,
}

impl std::fmt::Debug for OutboundWebhook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OutboundWebhook")
            .field("id", &self.id)
            .field("url", &self.url)
            .field("secret", &"<redacted>")
            .field("event_types", &self.event_types)
            .finish_non_exhaustive()
    }
}

/// A webhook to create.
#[derive(Debug, Clone)]
pub struct NewOutboundWebhook {
    pub url: String,
    pub secret: String,
    pub event_types: Vec<ScopedEventType>,
    pub created_by: Option<UserId>,
}

/// Changes to a webhook: its URL and event types are replaced, its secret when given.
#[derive(Debug, Clone)]
pub struct OutboundWebhookUpdate {
    pub url: String,
    pub secret: Option<String>,
    pub event_types: Vec<ScopedEventType>,
    pub updated_by: Option<UserId>,
}

/// Options of [`OutboundWebhookStore::list`].
#[derive(Debug, Clone, Default)]
pub struct ListOutboundWebhooks {
    /// Only webhooks subscribed to one of these event types (any scope).
    pub event_types: Option<Vec<String>>,
    /// Page size; no limit when 0.
    pub limit: i64,
    pub offset: i64,
}

/// The payload columns of an `outbound_webhook_jobs` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboundWebhookJob {
    /// Empty for a channel job.
    pub event_type: String,
    pub scope: Option<String>,
    /// The encrypted body; see [`OutboundWebhookStore::payload`].
    pub payload_enc: Vec<u8>,
    /// Set for a channel job.
    pub channel: Option<ChannelJob>,
}

/// The columns of a channel job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelJob {
    pub channel: Channel,
    /// The encrypted URL (the bot token for Telegram); see
    /// [`OutboundWebhookStore::channel_target`].
    pub target_enc: Vec<u8>,
    /// The encrypted signing key of a [`Channel::Webhook`] message.
    pub secret_enc: Option<Vec<u8>>,
    /// The watch action and trigger job the message belongs to.
    pub cm_action_id: Option<i64>,
    pub cm_trigger_job_id: Option<i64>,
}

impl<'r> sqlx::FromRow<'r, PgRow> for OutboundWebhookJob {
    fn from_row(r: &'r PgRow) -> sqlx::Result<Self> {
        let channel = match r.try_get::<Option<String>, _>("channel")? {
            None => None,
            Some(name) => Some(ChannelJob {
                channel: Channel::parse(&name).ok_or_else(|| sqlx::Error::ColumnDecode {
                    index: "channel".into(),
                    source: format!("unknown channel {name:?}").into(),
                })?,
                target_enc: r
                    .try_get::<Option<Vec<u8>>, _>("target_enc")?
                    .unwrap_or_default(),
                secret_enc: r.try_get("secret_enc")?,
                cm_action_id: r.try_get("cm_action_id")?,
                cm_trigger_job_id: r.try_get("cm_trigger_job_id")?,
            }),
        };
        Ok(OutboundWebhookJob {
            event_type: r
                .try_get::<Option<String>, _>("event_type")?
                .unwrap_or_default(),
            scope: r.try_get("scope")?,
            payload_enc: r.try_get("payload_enc")?,
            channel,
        })
    }
}

/// A channel job to queue: one rendered message for one target.
#[derive(Clone, Copy)]
pub struct NewChannelJob<'a> {
    pub channel: Channel,
    /// The URL; for [`Channel::Telegram`], the bot token.
    pub target: &'a str,
    /// [`Channel::Webhook`]: signs the body with this key.
    pub secret: Option<&'a str>,
    /// The rendered JSON body.
    pub body: &'a [u8],
    /// The watch action and trigger job the message belongs to; at most one job is queued
    /// per pair.
    pub cm_action_id: Option<i64>,
    pub cm_trigger_job_id: Option<i64>,
}

impl std::fmt::Debug for NewChannelJob<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NewChannelJob")
            .field("channel", &self.channel)
            .field("cm_action_id", &self.cm_action_id)
            .field("cm_trigger_job_id", &self.cm_trigger_job_id)
            .finish_non_exhaustive()
    }
}

/// A job with its queue state.
pub type OutboundWebhookJobRow = Record<OutboundWebhookJob>;

/// The request of a delivery attempt, as logged.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LoggedRequest {
    pub method: String,
    pub url: String,
    /// HTTP version, e.g. `HTTP/1.1`.
    pub version: String,
    /// Header values by name.
    pub headers: BTreeMap<String, Vec<String>>,
    pub body: String,
}

/// The response to a delivery attempt, as logged (body truncated).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LoggedResponse {
    pub status_code: i32,
    pub version: String,
    pub headers: BTreeMap<String, Vec<String>>,
    pub body: String,
}

/// One delivery attempt to record.
#[derive(Debug, Clone)]
pub struct NewOutboundWebhookLog {
    pub job_id: i64,
    pub outbound_webhook_id: i64,
    /// 0 when no response arrived.
    pub status_code: i32,
    pub request: LoggedRequest,
    pub response: Option<LoggedResponse>,
    pub error: Option<String>,
}

/// One delivery attempt through a channel, to record: of a channel job (`job_id`) or of a
/// watch's action job (`cm_action_job_id`); exactly one is set.
#[derive(Debug, Clone)]
pub struct NewChannelLog {
    pub job_id: Option<i64>,
    pub cm_action_job_id: Option<i64>,
    /// The channel, e.g. `webhook`, `slack_webhook`, `telegram`.
    pub channel: String,
    /// 0 when no response arrived.
    pub status_code: i32,
    pub request: LoggedRequest,
    pub response: Option<LoggedResponse>,
    pub error: Option<String>,
}

/// One delivery attempt through a channel, decrypted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelLog {
    pub id: i64,
    /// The channel job, or
    pub job_id: Option<i64>,
    /// the watch's action job.
    pub cm_action_job_id: Option<i64>,
    pub channel: String,
    pub sent_at: DateTime<Utc>,
    pub status_code: i32,
    pub request: LoggedRequest,
    pub response: Option<LoggedResponse>,
    pub error: Option<String>,
}

/// One delivery attempt through a channel with the watch it was for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelDelivery {
    pub log: ChannelLog,
    /// The watch, unless it was deleted.
    pub monitor_id: Option<i64>,
    pub monitor_description: Option<String>,
}

/// One delivery attempt, decrypted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboundWebhookLog {
    pub id: i64,
    pub job_id: i64,
    pub outbound_webhook_id: i64,
    pub sent_at: DateTime<Utc>,
    pub status_code: i32,
    pub request: LoggedRequest,
    pub response: Option<LoggedResponse>,
    pub error: Option<String>,
}

impl OutboundWebhookLog {
    /// Whether the attempt failed (no 2xx response).
    pub fn is_error(&self) -> bool {
        !(200..300).contains(&self.status_code)
    }
}

/// Counts of a webhook's delivery attempts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OutboundWebhookLogStats {
    pub total: i64,
    /// Attempts without a 2xx response.
    pub errored: i64,
}

#[derive(sqlx::FromRow)]
struct WebhookRow {
    id: i64,
    url_enc: Vec<u8>,
    secret_enc: Vec<u8>,
    created_by: Option<UserId>,
    created_at: DateTime<Utc>,
    updated_by: Option<UserId>,
    updated_at: DateTime<Utc>,
}

#[derive(sqlx::FromRow)]
struct LogRow {
    id: i64,
    job_id: i64,
    outbound_webhook_id: i64,
    sent_at: DateTime<Utc>,
    status_code: i32,
    request_enc: Vec<u8>,
    response_enc: Option<Vec<u8>>,
    error_enc: Option<Vec<u8>>,
}

#[derive(sqlx::FromRow)]
struct ChannelLogRow {
    id: i64,
    job_id: Option<i64>,
    cm_action_job_id: Option<i64>,
    channel: String,
    sent_at: DateTime<Utc>,
    status_code: i32,
    request_enc: Vec<u8>,
    response_enc: Option<Vec<u8>>,
    error_enc: Option<Vec<u8>>,
}

#[derive(sqlx::FromRow)]
struct ChannelDeliveryRow {
    #[sqlx(flatten)]
    log: ChannelLogRow,
    monitor_id: Option<i64>,
    monitor_description: Option<String>,
}

/// `FROM` clause of channel attempts `l` with their watch `m`, through the channel job or the
/// watch's action job.
const CHANNEL_DELIVERIES: &str = "outbound_webhook_logs l
     LEFT JOIN outbound_webhook_jobs j ON j.id = l.job_id
     LEFT JOIN cm_action_jobs aj ON aj.id = l.cm_action_job_id
     LEFT JOIN cm_actions a ON a.id = COALESCE(j.cm_action_id, aj.action_id)
     LEFT JOIN cm_monitors m ON m.id = a.monitor_id";

const WEBHOOK_COLUMNS: &str =
    "w.id, w.url_enc, w.secret_enc, w.created_by, w.created_at, w.updated_by, w.updated_at";

/// `WHERE` clause of [`ListOutboundWebhooks`] over `outbound_webhooks w`, binds `$1`.
const LIST_FILTER: &str = "($1::text[] IS NULL OR EXISTS (
         SELECT 1 FROM outbound_webhook_event_types e
         WHERE e.outbound_webhook_id = w.id AND e.event_type = ANY($1)))";

/// Matches subscriptions to event type `$1` and scope `$2` over `outbound_webhook_event_types e`.
const SUBSCRIBED: &str = "e.event_type = $1 AND (e.scope IS NULL OR e.scope = $2::text)";

/// Reads and writes outbound webhooks, their event queue and delivery logs.
#[derive(Debug, Clone)]
pub struct OutboundWebhookStore {
    pool: PgPool,
    keyring: Option<Arc<Keyring>>,
}

impl OutboundWebhookStore {
    /// Creates a store on `db`. URLs, secrets, payloads and logs are encrypted, so most calls
    /// need `keyring`.
    pub fn new(db: &Db, keyring: Option<Arc<Keyring>>) -> Self {
        Self {
            pool: db.pool().clone(),
            keyring,
        }
    }

    /// The settings of the `outbound_webhook_jobs` queue: five attempts, 30 s backoff doubling
    /// up to 10 min (an event is retried for about 7.5 minutes).
    pub fn queue_config() -> QueueConfig {
        let mut config = QueueConfig::new(OUTBOUND_WEBHOOK_JOBS_TABLE);
        config.max_failures = 5;
        config.backoff_base = std::time::Duration::from_secs(30);
        config.backoff_max = std::time::Duration::from_secs(600);
        config
    }

    fn keyring(&self) -> Result<&Keyring> {
        self.keyring.as_deref().ok_or(DbError::Invalid(
            "encryption keys are not configured (set SOURCEGULL_KEYS_FILE or SOURCEGULL_KEYS)",
        ))
    }

    fn encrypt(&self, plain: &[u8]) -> Result<Vec<u8>> {
        Ok(self
            .keyring()?
            .encrypt(RecordClass::OutboundWebhook, plain)?)
    }

    fn encrypt_json<T: Serialize>(&self, value: &T) -> Result<Vec<u8>> {
        let bytes = serde_json::to_vec(value)
            .map_err(|_| DbError::Invalid("an outbound webhook log is not serializable"))?;
        self.encrypt(&bytes)
    }

    fn decrypt(&self, blob: &[u8]) -> Result<Vec<u8>> {
        Ok(self
            .keyring()?
            .decrypt(RecordClass::OutboundWebhook, blob)?
            .to_vec())
    }

    fn decrypt_string(&self, blob: &[u8]) -> Result<String> {
        String::from_utf8(self.decrypt(blob)?)
            .map_err(|_| DbError::Invalid("an encrypted outbound webhook field is not UTF-8"))
    }

    fn decrypt_json<T: for<'de> Deserialize<'de>>(&self, blob: &[u8]) -> Result<T> {
        serde_json::from_slice(&self.decrypt(blob)?)
            .map_err(|_| DbError::Invalid("an encrypted outbound webhook log is not valid JSON"))
    }

    async fn hydrate(&self, rows: Vec<WebhookRow>) -> Result<Vec<OutboundWebhook>> {
        let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
        let types: Vec<(i64, String, Option<String>)> = sqlx::query_as(
            "SELECT outbound_webhook_id, event_type, scope FROM outbound_webhook_event_types
             WHERE outbound_webhook_id = ANY($1) ORDER BY event_type, scope NULLS FIRST, id",
        )
        .bind(&ids)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|r| {
                Ok(OutboundWebhook {
                    id: r.id,
                    url: self.decrypt_string(&r.url_enc)?,
                    secret: self.decrypt_string(&r.secret_enc)?,
                    event_types: types
                        .iter()
                        .filter(|(id, _, _)| *id == r.id)
                        .map(|(_, event_type, scope)| ScopedEventType {
                            event_type: event_type.clone(),
                            scope: scope.clone(),
                        })
                        .collect(),
                    created_by: r.created_by,
                    created_at: r.created_at,
                    updated_by: r.updated_by,
                    updated_at: r.updated_at,
                })
            })
            .collect()
    }

    async fn set_event_types(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        id: i64,
        event_types: &[ScopedEventType],
    ) -> Result<()> {
        let mut unique: Vec<&ScopedEventType> = event_types.iter().collect();
        unique.sort();
        unique.dedup();
        sqlx::query("DELETE FROM outbound_webhook_event_types WHERE outbound_webhook_id = $1")
            .bind(id)
            .execute(&mut **tx)
            .await?;
        let types: Vec<&str> = unique.iter().map(|t| t.event_type.as_str()).collect();
        let scopes: Vec<Option<&str>> = unique.iter().map(|t| t.scope.as_deref()).collect();
        sqlx::query(
            "INSERT INTO outbound_webhook_event_types (outbound_webhook_id, event_type, scope)
             SELECT $1, t, s FROM unnest($2::text[], $3::text[]) AS u(t, s)",
        )
        .bind(id)
        .bind(&types)
        .bind(&scopes)
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    /// Creates a webhook.
    pub async fn create(&self, new: &NewOutboundWebhook) -> Result<OutboundWebhook> {
        let url = self.encrypt(new.url.as_bytes())?;
        let secret = self.encrypt(new.secret.as_bytes())?;
        let mut tx = self.pool.begin().await?;
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO outbound_webhooks (created_by, updated_by, url_enc, secret_enc)
             VALUES ($1, $1, $2, $3) RETURNING id",
        )
        .bind(new.created_by)
        .bind(url)
        .bind(secret)
        .fetch_one(&mut *tx)
        .await?;
        Self::set_event_types(&mut tx, id, &new.event_types).await?;
        tx.commit().await?;
        self.get(id)
            .await?
            .ok_or(DbError::NotFound("outbound webhook"))
    }

    /// Looks up a webhook.
    pub async fn get(&self, id: i64) -> Result<Option<OutboundWebhook>> {
        let rows: Vec<WebhookRow> = sqlx::query_as(&format!(
            "SELECT {WEBHOOK_COLUMNS} FROM outbound_webhooks w WHERE w.id = $1"
        ))
        .bind(id)
        .fetch_all(&self.pool)
        .await?;
        Ok(self.hydrate(rows).await?.pop())
    }

    /// Webhooks in id order.
    pub async fn list(&self, opts: &ListOutboundWebhooks) -> Result<Vec<OutboundWebhook>> {
        let rows: Vec<WebhookRow> = sqlx::query_as(&format!(
            "SELECT {WEBHOOK_COLUMNS} FROM outbound_webhooks w WHERE {LIST_FILTER}
             ORDER BY w.id LIMIT $2 OFFSET $3"
        ))
        .bind(&opts.event_types)
        .bind((opts.limit > 0).then_some(opts.limit))
        .bind(opts.offset)
        .fetch_all(&self.pool)
        .await?;
        self.hydrate(rows).await
    }

    /// Number of webhooks [`list`](Self::list) would return without paging.
    pub async fn count(&self, opts: &ListOutboundWebhooks) -> Result<i64> {
        Ok(sqlx::query_scalar(&format!(
            "SELECT count(*) FROM outbound_webhooks w WHERE {LIST_FILTER}"
        ))
        .bind(&opts.event_types)
        .fetch_one(&self.pool)
        .await?)
    }

    /// Updates a webhook; `None` if there is no such webhook.
    pub async fn update(
        &self,
        id: i64,
        update: &OutboundWebhookUpdate,
    ) -> Result<Option<OutboundWebhook>> {
        let url = self.encrypt(update.url.as_bytes())?;
        let secret = update
            .secret
            .as_ref()
            .map(|s| self.encrypt(s.as_bytes()))
            .transpose()?;
        let mut tx = self.pool.begin().await?;
        let updated = sqlx::query(
            "UPDATE outbound_webhooks
             SET url_enc = $2, secret_enc = COALESCE($3, secret_enc), updated_by = $4,
                 updated_at = now()
             WHERE id = $1",
        )
        .bind(id)
        .bind(url)
        .bind(secret)
        .bind(update.updated_by)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if updated == 0 {
            return Ok(None);
        }
        Self::set_event_types(&mut tx, id, &update.event_types).await?;
        tx.commit().await?;
        self.get(id).await
    }

    /// Deletes a webhook with its subscriptions and logs; false if there was none.
    pub async fn delete(&self, id: i64) -> Result<bool> {
        Ok(sqlx::query("DELETE FROM outbound_webhooks WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?
            .rows_affected()
            > 0)
    }

    /// Whether any webhook subscribes to `event_type` in `scope` (no decryption, so callers
    /// can skip building payloads nobody receives).
    pub async fn has_subscribers(&self, event_type: &str, scope: Option<&str>) -> Result<bool> {
        Ok(sqlx::query_scalar(&format!(
            "SELECT EXISTS (SELECT 1 FROM outbound_webhook_event_types e WHERE {SUBSCRIBED})"
        ))
        .bind(event_type)
        .bind(scope)
        .fetch_one(&self.pool)
        .await?)
    }

    /// The webhooks subscribed to `event_type` in `scope`, in id order.
    pub async fn subscribers(
        &self,
        event_type: &str,
        scope: Option<&str>,
    ) -> Result<Vec<OutboundWebhook>> {
        let rows: Vec<WebhookRow> = sqlx::query_as(&format!(
            "SELECT {WEBHOOK_COLUMNS} FROM outbound_webhooks w
             WHERE EXISTS (SELECT 1 FROM outbound_webhook_event_types e
                           WHERE e.outbound_webhook_id = w.id AND {SUBSCRIBED})
             ORDER BY w.id"
        ))
        .bind(event_type)
        .bind(scope)
        .fetch_all(&self.pool)
        .await?;
        self.hydrate(rows).await
    }

    /// Queues an event with its (JSON) body and returns the job id.
    pub async fn enqueue(
        &self,
        event_type: &str,
        scope: Option<&str>,
        payload: &[u8],
    ) -> Result<i64> {
        let enc = self.encrypt(payload)?;
        let mut tx = self.pool.begin().await?;
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO outbound_webhook_jobs (event_type, scope, payload_enc)
             VALUES ($1, $2, $3) RETURNING id",
        )
        .bind(event_type)
        .bind(scope)
        .bind(enc)
        .fetch_one(&mut *tx)
        .await?;
        notify_enqueued(&mut *tx, OUTBOUND_WEBHOOK_JOBS_TABLE).await?;
        tx.commit().await?;
        Ok(id)
    }

    /// Loads a job.
    pub async fn job(&self, id: i64) -> Result<Option<OutboundWebhookJobRow>> {
        Ok(
            sqlx::query_as("SELECT * FROM outbound_webhook_jobs WHERE id = $1")
                .bind(id)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    /// The decrypted body of a job.
    pub fn payload(&self, job: &OutboundWebhookJob) -> Result<Vec<u8>> {
        self.decrypt(&job.payload_enc)
    }

    /// Queues a channel job on `conn` (normally the caller's transaction, so the job is
    /// queued with what produced it) and returns its id, or `None` when the watch action
    /// already has a job for the trigger job.
    pub async fn enqueue_channel(
        &self,
        conn: &mut PgConnection,
        job: &NewChannelJob<'_>,
    ) -> Result<Option<i64>> {
        let target = self.encrypt(job.target.as_bytes())?;
        let secret = job.secret.map(|s| self.encrypt(s.as_bytes())).transpose()?;
        let body = self.encrypt(job.body)?;
        let id: Option<i64> = sqlx::query_scalar(
            "INSERT INTO outbound_webhook_jobs
                 (channel, target_enc, secret_enc, payload_enc, cm_action_id, cm_trigger_job_id)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (cm_action_id, cm_trigger_job_id) WHERE cm_action_id IS NOT NULL
             DO NOTHING RETURNING id",
        )
        .bind(job.channel.as_str())
        .bind(target)
        .bind(secret)
        .bind(body)
        .bind(job.cm_action_id)
        .bind(job.cm_trigger_job_id)
        .fetch_optional(&mut *conn)
        .await?;
        if id.is_some() {
            notify_enqueued(&mut *conn, OUTBOUND_WEBHOOK_JOBS_TABLE).await?;
        }
        Ok(id)
    }

    /// Cancels the channel jobs of watch actions `cm_action_ids` that wait for their first
    /// attempt or a retry, with `message` as their failure message. Returns how many.
    pub async fn cancel_channel_jobs(
        conn: &mut PgConnection,
        cm_action_ids: &[i64],
        message: &str,
    ) -> Result<u64> {
        if cm_action_ids.is_empty() {
            return Ok(0);
        }
        Ok(sqlx::query(
            "UPDATE outbound_webhook_jobs
             SET state = 'canceled', finished_at = now(), failure_message = $2
             WHERE cm_action_id = ANY($1) AND state IN ('queued', 'errored')",
        )
        .bind(cm_action_ids)
        .bind(message)
        .execute(&mut *conn)
        .await?
        .rows_affected())
    }

    /// The decrypted target and secret of a channel job.
    pub fn channel_target(&self, job: &ChannelJob) -> Result<(String, Option<String>)> {
        Ok((
            self.decrypt_string(&job.target_enc)?,
            job.secret_enc
                .as_deref()
                .map(|s| self.decrypt_string(s))
                .transpose()?,
        ))
    }

    /// Records a delivery attempt and returns its id.
    pub async fn create_log(&self, log: &NewOutboundWebhookLog) -> Result<i64> {
        let request = self.encrypt_json(&log.request)?;
        let response = log
            .response
            .as_ref()
            .map(|r| self.encrypt_json(r))
            .transpose()?;
        let error = log
            .error
            .as_ref()
            .map(|e| self.encrypt(e.as_bytes()))
            .transpose()?;
        Ok(sqlx::query_scalar(
            "INSERT INTO outbound_webhook_logs
                 (job_id, outbound_webhook_id, status_code, request_enc, response_enc, error_enc)
             VALUES ($1, $2, $3, $4, $5, $6) RETURNING id",
        )
        .bind(log.job_id)
        .bind(log.outbound_webhook_id)
        .bind(log.status_code)
        .bind(request)
        .bind(response)
        .bind(error)
        .fetch_one(&self.pool)
        .await?)
    }

    /// Records a delivery attempt through a channel and returns its id.
    pub async fn create_channel_log(&self, log: &NewChannelLog) -> Result<i64> {
        let request = self.encrypt_json(&log.request)?;
        let response = log
            .response
            .as_ref()
            .map(|r| self.encrypt_json(r))
            .transpose()?;
        let error = log
            .error
            .as_ref()
            .map(|e| self.encrypt(e.as_bytes()))
            .transpose()?;
        Ok(sqlx::query_scalar(
            "INSERT INTO outbound_webhook_logs
                 (job_id, cm_action_job_id, channel, status_code, request_enc, response_enc,
                  error_enc)
             VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING id",
        )
        .bind(log.job_id)
        .bind(log.cm_action_job_id)
        .bind(&log.channel)
        .bind(log.status_code)
        .bind(request)
        .bind(response)
        .bind(error)
        .fetch_one(&self.pool)
        .await?)
    }

    /// The delivery attempts of a watch's action job, oldest first.
    pub async fn channel_logs(&self, cm_action_job_id: i64) -> Result<Vec<ChannelLog>> {
        self.channel_logs_where("cm_action_job_id = $1", cm_action_job_id)
            .await
    }

    /// The delivery attempts of a channel job, oldest first.
    pub async fn channel_job_logs(&self, job_id: i64) -> Result<Vec<ChannelLog>> {
        self.channel_logs_where("job_id = $1 AND outbound_webhook_id IS NULL", job_id)
            .await
    }

    /// The channel attempts matching `filter`, which binds `$1` to `id`, oldest first.
    async fn channel_logs_where(&self, filter: &str, id: i64) -> Result<Vec<ChannelLog>> {
        let rows: Vec<ChannelLogRow> = sqlx::query_as(&format!(
            "SELECT id, job_id, cm_action_job_id, channel, sent_at, status_code, request_enc,
                 response_enc, error_enc
             FROM outbound_webhook_logs WHERE {filter} ORDER BY sent_at, id"
        ))
        .bind(id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|r| self.decode_channel_log(r))
            .collect()
    }

    fn decode_channel_log(&self, r: ChannelLogRow) -> Result<ChannelLog> {
        Ok(ChannelLog {
            id: r.id,
            job_id: r.job_id,
            cm_action_job_id: r.cm_action_job_id,
            channel: r.channel,
            sent_at: r.sent_at,
            status_code: r.status_code,
            request: self.decrypt_json(&r.request_enc)?,
            response: r
                .response_enc
                .as_deref()
                .map(|b| self.decrypt_json(b))
                .transpose()?,
            error: r
                .error_enc
                .as_deref()
                .map(|b| self.decrypt_string(b))
                .transpose()?,
        })
    }

    /// Every delivery attempt through a channel, newest first, with its watch; only failed
    /// ones with `only_errors`. No limit when `limit` is 0.
    pub async fn channel_deliveries(
        &self,
        only_errors: bool,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<ChannelDelivery>> {
        let rows: Vec<ChannelDeliveryRow> = sqlx::query_as(&format!(
            "SELECT l.id, l.job_id, l.cm_action_job_id, l.channel, l.sent_at, l.status_code,
                 l.request_enc, l.response_enc, l.error_enc, m.id AS monitor_id,
                 m.description AS monitor_description
             FROM {CHANNEL_DELIVERIES}
             WHERE l.outbound_webhook_id IS NULL
               AND (NOT $1 OR l.status_code NOT BETWEEN 200 AND 299)
             ORDER BY l.sent_at DESC, l.id DESC LIMIT $2 OFFSET $3"
        ))
        .bind(only_errors)
        .bind((limit > 0).then_some(limit))
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|r| {
                Ok(ChannelDelivery {
                    log: self.decode_channel_log(r.log)?,
                    monitor_id: r.monitor_id,
                    monitor_description: r.monitor_description,
                })
            })
            .collect()
    }

    /// Counts the delivery attempts through channels.
    pub async fn channel_delivery_stats(&self) -> Result<OutboundWebhookLogStats> {
        let (total, errored): (i64, i64) = sqlx::query_as(
            "SELECT count(*), count(*) FILTER (WHERE status_code NOT BETWEEN 200 AND 299)
             FROM outbound_webhook_logs WHERE outbound_webhook_id IS NULL",
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(OutboundWebhookLogStats { total, errored })
    }

    fn decode_log(&self, r: LogRow) -> Result<OutboundWebhookLog> {
        Ok(OutboundWebhookLog {
            id: r.id,
            job_id: r.job_id,
            outbound_webhook_id: r.outbound_webhook_id,
            sent_at: r.sent_at,
            status_code: r.status_code,
            request: self.decrypt_json(&r.request_enc)?,
            response: r
                .response_enc
                .as_deref()
                .map(|b| self.decrypt_json(b))
                .transpose()?,
            error: r
                .error_enc
                .as_deref()
                .map(|b| self.decrypt_string(b))
                .transpose()?,
        })
    }

    /// The webhooks a job was delivered to (a 2xx response), so retries skip them.
    pub async fn delivered(&self, job_id: i64) -> Result<HashSet<i64>> {
        let ids: Vec<i64> = sqlx::query_scalar(
            "SELECT DISTINCT outbound_webhook_id FROM outbound_webhook_logs
             WHERE job_id = $1 AND status_code BETWEEN 200 AND 299",
        )
        .bind(job_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(ids.into_iter().collect())
    }

    /// A webhook's delivery attempts, newest first; only failed ones with `only_errors`.
    /// No limit when `limit` is 0.
    pub async fn logs(
        &self,
        webhook_id: i64,
        only_errors: bool,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<OutboundWebhookLog>> {
        let rows: Vec<LogRow> = sqlx::query_as(
            "SELECT * FROM outbound_webhook_logs
             WHERE outbound_webhook_id = $1
               AND (NOT $2 OR status_code NOT BETWEEN 200 AND 299)
             ORDER BY sent_at DESC, id DESC LIMIT $3 OFFSET $4",
        )
        .bind(webhook_id)
        .bind(only_errors)
        .bind((limit > 0).then_some(limit))
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(|r| self.decode_log(r)).collect()
    }

    /// The delivery attempts of an event to webhooks, oldest first.
    pub async fn job_logs(&self, job_id: i64) -> Result<Vec<OutboundWebhookLog>> {
        let rows: Vec<LogRow> = sqlx::query_as(
            "SELECT * FROM outbound_webhook_logs
             WHERE job_id = $1 AND outbound_webhook_id IS NOT NULL ORDER BY sent_at, id",
        )
        .bind(job_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(|r| self.decode_log(r)).collect()
    }

    /// Looks up a delivery attempt to a webhook.
    pub async fn log(&self, id: i64) -> Result<Option<OutboundWebhookLog>> {
        let row: Option<LogRow> = sqlx::query_as(
            "SELECT * FROM outbound_webhook_logs WHERE id = $1 AND outbound_webhook_id IS NOT NULL",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|r| self.decode_log(r)).transpose()
    }

    /// Counts a webhook's delivery attempts.
    pub async fn log_stats(&self, webhook_id: i64) -> Result<OutboundWebhookLogStats> {
        let (total, errored): (i64, i64) = sqlx::query_as(
            "SELECT count(*), count(*) FILTER (WHERE status_code NOT BETWEEN 200 AND 299)
             FROM outbound_webhook_logs WHERE outbound_webhook_id = $1",
        )
        .bind(webhook_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(OutboundWebhookLogStats { total, errored })
    }

    /// Deletes delivery logs sent before `before`, and finished jobs (with their logs) that
    /// finished before it. Channel jobs of a watch trigger job are kept: they are the watch's
    /// action log and are deleted with their trigger job. Returns the numbers of logs and
    /// jobs deleted.
    pub async fn delete_older_than(&self, before: DateTime<Utc>) -> Result<(u64, u64)> {
        let logs = sqlx::query("DELETE FROM outbound_webhook_logs WHERE sent_at < $1")
            .bind(before)
            .execute(&self.pool)
            .await?
            .rows_affected();
        let jobs = sqlx::query(
            "DELETE FROM outbound_webhook_jobs
             WHERE finished_at < $1 AND state IN ('completed', 'failed', 'canceled')
               AND cm_trigger_job_id IS NULL",
        )
        .bind(before)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok((logs, jobs))
    }
}
