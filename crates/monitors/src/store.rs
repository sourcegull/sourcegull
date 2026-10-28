//! The watch tables (`cm_*`, migration `20261112000001_code_monitors`): watches with
//! their trigger query, actions and email recipients, the last commit searched per watch,
//! repository and revision, the trigger and action job queues (the run and action logs) and
//! the evaluator's cursor over `repo_ref_events`. HTTP actions' messages are queued as channel
//! jobs on `outbound_webhook_jobs` (through [`OutboundWebhookStore`]), which are part of the
//! action log too.
//!
//! Writes that change what the evaluator watches send `NOTIFY` [`CHANNEL`] on commit.
//!
//! Webhook URLs and Telegram bot tokens (`cm_actions.url`) and email headers
//! (`cm_actions.header`, which can carry a mailing list password) are encrypted with the
//! `code_monitor_webhook` data key (`cm_actions.url_enc`, `cm_actions.header_enc`) when the
//! store has a keyring ([`MonitorStore::with_keyring`]); without one they are stored in
//! plaintext, as are rows written before encryption, until out-of-band migration 2 encrypts
//! them. Reading an encrypted value needs the keyring.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sourcegull_crypto::{Keyring, RecordClass};
use sourcegull_db::notify::notify;
use sourcegull_db::{Db, DbError, Fence, Namespace, Result};
use sourcegull_outbound_webhooks::Channel;
use sourcegull_outbound_webhooks::store::{NewChannelJob, OutboundWebhookStore};
use sourcegull_pgqueue::QueueConfig;
use sourcegull_types::{OrgId, RepoId, UserId};
use sqlx::postgres::PgRow;
use sqlx::types::Json;
use sqlx::{PgPool, Postgres, Row, Transaction};

use crate::results::MonitorResult;

/// `NOTIFY` channel: watches were created, changed or deleted. Payload: the watch id.
pub const CHANNEL: &str = "code_monitors";
/// Queue table of trigger jobs.
pub const TRIGGER_JOBS: &str = "cm_trigger_jobs";
/// Queue table of action jobs.
pub const ACTION_JOBS: &str = "cm_action_jobs";

/// A watch with its trigger query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Monitor {
    pub id: i64,
    pub created_by: Option<UserId>,
    pub created_at: DateTime<Utc>,
    pub updated_by: Option<UserId>,
    pub updated_at: DateTime<Utc>,
    pub description: String,
    pub enabled: bool,
    pub namespace: Namespace,
    pub query: String,
    pub query_updated_at: DateTime<Utc>,
    /// The next evaluation searches whole histories (`resetTriggerQueryTimestamps`).
    pub search_all: bool,
    /// Who made the email recipients get every result unfiltered, and when; `None`: each
    /// recipient gets the results they can read. Cleared when someone else takes the watch
    /// over (see [`crate::access::unfiltered_owner`]).
    pub unfiltered_by: Option<UserId>,
    pub unfiltered_at: Option<DateTime<Utc>>,
}

const MONITOR_COLUMNS: &str = "id, created_by, created_at, updated_by, updated_at, description, \
     enabled, namespace_user_id, namespace_org_id, query, query_updated_at, search_all, \
     unfiltered_by, unfiltered_at";

fn monitor_from_row(r: &PgRow) -> sqlx::Result<Monitor> {
    let user: Option<UserId> = r.try_get("namespace_user_id")?;
    let org: Option<OrgId> = r.try_get("namespace_org_id")?;
    let namespace = match (user, org) {
        (Some(u), _) => Namespace::User(u),
        (None, Some(o)) => Namespace::Org(o),
        (None, None) => {
            return Err(sqlx::Error::ColumnDecode {
                index: "namespace_user_id".into(),
                source: "a watch without a namespace".into(),
            });
        }
    };
    Ok(Monitor {
        id: r.try_get("id")?,
        created_by: r.try_get("created_by")?,
        created_at: r.try_get("created_at")?,
        updated_by: r.try_get("updated_by")?,
        updated_at: r.try_get("updated_at")?,
        description: r.try_get("description")?,
        enabled: r.try_get("enabled")?,
        namespace,
        query: r.try_get("query")?,
        query_updated_at: r.try_get("query_updated_at")?,
        search_all: r.try_get("search_all")?,
        unfiltered_by: r.try_get("unfiltered_by")?,
        unfiltered_at: r.try_get("unfiltered_at")?,
    })
}

fn namespace_columns(ns: Namespace) -> (Option<UserId>, Option<OrgId>) {
    match ns {
        Namespace::User(u) => (Some(u), None),
        Namespace::Org(o) => (None, Some(o)),
    }
}

/// What an action does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ActionKind {
    Email,
    Webhook,
    SlackWebhook,
    MattermostWebhook,
    Telegram,
}

impl ActionKind {
    /// The stored form.
    pub const fn as_str(self) -> &'static str {
        match self {
            ActionKind::Email => "email",
            ActionKind::Webhook => "webhook",
            ActionKind::SlackWebhook => "slack_webhook",
            ActionKind::MattermostWebhook => "mattermost_webhook",
            ActionKind::Telegram => "telegram",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "email" => ActionKind::Email,
            "webhook" => ActionKind::Webhook,
            "slack_webhook" => ActionKind::SlackWebhook,
            "mattermost_webhook" => ActionKind::MattermostWebhook,
            "telegram" => ActionKind::Telegram,
            _ => return None,
        })
    }
}

/// The priority of an email action (a critical one says so in the subject).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Priority {
    #[default]
    Normal,
    Critical,
}

impl Priority {
    const fn as_str(self) -> &'static str {
        match self {
            Priority::Normal => "NORMAL",
            Priority::Critical => "CRITICAL",
        }
    }
}

/// An action's settings, as created or updated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionSpec {
    pub kind: ActionKind,
    pub enabled: bool,
    pub include_results: bool,
    /// Webhook, Slack and Mattermost webhook actions: the URL. Telegram actions: the bot token.
    pub url: String,
    /// Telegram actions: the chat (numeric ID or `@channel`).
    pub chat_id: String,
    /// Email actions.
    pub priority: Priority,
    pub header: String,
    /// Email actions: users, or every member of an org.
    pub recipients: Vec<Namespace>,
}

/// A stored action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Action {
    pub id: i64,
    pub monitor_id: i64,
    pub spec: ActionSpec,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// One action of an update: `id: None` creates it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionEdit {
    pub id: Option<i64>,
    pub spec: ActionSpec,
}

/// A new watch.
#[derive(Debug, Clone)]
pub struct NewMonitor {
    pub namespace: Namespace,
    pub description: String,
    pub enabled: bool,
    pub query: String,
    pub actions: Vec<ActionSpec>,
    pub created_by: UserId,
    /// Email recipients get every result unfiltered (see [`Monitor::unfiltered_by`]).
    pub unfiltered: bool,
}

/// The editable fields of a watch.
#[derive(Debug, Clone)]
pub struct MonitorUpdate {
    pub namespace: Namespace,
    pub description: String,
    pub enabled: bool,
    pub query: String,
    /// The complete list: stored actions missing here are deleted.
    pub actions: Vec<ActionEdit>,
    pub updated_by: UserId,
    /// Email recipients get every result unfiltered, as `updated_by` shares them.
    pub unfiltered: bool,
}

/// A commit range a trigger job searches: what a ref moved from `from` to `to`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommitRange {
    pub repo_id: i32,
    pub repo: String,
    /// The watched revision; `""` is the default branch.
    pub rev: String,
    /// `None` searches the whole history up to `to`.
    pub from: Option<String>,
    pub to: String,
}

/// A trigger job's payload columns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TriggerJob {
    pub monitor_id: i64,
    pub ranges: Vec<CommitRange>,
    pub query: String,
}

impl<'r> sqlx::FromRow<'r, PgRow> for TriggerJob {
    fn from_row(r: &'r PgRow) -> sqlx::Result<Self> {
        let Json(ranges) = r.try_get("ranges")?;
        Ok(TriggerJob {
            monitor_id: r.try_get("monitor_id")?,
            ranges,
            query: r.try_get("query")?,
        })
    }
}

/// An action job's payload columns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::FromRow)]
pub struct ActionJob {
    pub action_id: i64,
    pub trigger_job_id: i64,
}

/// One evaluation of a watch (the run log).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TriggerEvent {
    pub id: i64,
    pub monitor_id: i64,
    /// The pgqueue state (`queued`, `processing`, `completed`, `failed`, …).
    pub state: String,
    pub failure_message: Option<String>,
    pub queued_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub query: String,
    pub ranges: Vec<CommitRange>,
    pub num_results: i32,
    /// `None` until the search finished.
    pub results: Option<Vec<MonitorResult>>,
}

const TRIGGER_COLUMNS: &str = "id, monitor_id, state, failure_message, queued_at, finished_at, \
     query, ranges, num_results, results";

fn trigger_event_from_row(r: &PgRow) -> sqlx::Result<TriggerEvent> {
    let Json(ranges) = r.try_get("ranges")?;
    let results: Option<Json<Vec<MonitorResult>>> = r.try_get("results")?;
    Ok(TriggerEvent {
        id: r.try_get("id")?,
        monitor_id: r.try_get("monitor_id")?,
        state: r.try_get("state")?,
        failure_message: r.try_get("failure_message")?,
        queued_at: r.try_get("queued_at")?,
        finished_at: r.try_get("finished_at")?,
        query: r.try_get("query")?,
        ranges,
        num_results: r.try_get("num_results")?,
        results: results.map(|Json(r)| r),
    })
}

/// The rendered message of an HTTP action for a trigger job, to queue as a channel job.
#[derive(Clone)]
pub struct ChannelAction {
    pub action_id: i64,
    pub channel: Channel,
    /// The URL; for Telegram, the bot token.
    pub target: String,
    /// The JSON body.
    pub body: Vec<u8>,
}

impl std::fmt::Debug for ChannelAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChannelAction")
            .field("action_id", &self.action_id)
            .field("channel", &self.channel)
            .finish_non_exhaustive()
    }
}

/// The failure message of a channel job canceled because its watch or action was turned off.
pub const TURNED_OFF: &str = "The watch or the action was turned off before the delivery.";

/// One delivery through an action (the action log): an action job, or a channel job of the
/// outbound webhook queue.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct ActionEvent {
    /// The id of the action job, or of the channel job when `channel_job`.
    pub id: i64,
    pub channel_job: bool,
    pub action_id: i64,
    pub trigger_job_id: i64,
    pub state: String,
    pub failure_message: Option<String>,
    pub num_failures: i32,
    pub queued_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
}

/// A row of `cm_last_searched`.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct LastSearched {
    pub monitor_id: i64,
    pub repo_id: RepoId,
    pub rev: String,
    pub commit_oid: String,
    pub checked_at: DateTime<Utc>,
}

/// Which watches [`MonitorStore::list`] returns.
#[derive(Debug, Clone, Default)]
pub struct ListMonitors {
    /// Watches owned by this user or by an org they belong to; `None` lists all.
    pub affiliated_user: Option<UserId>,
    pub limit: i64,
    pub offset: i64,
}

/// What the evaluator does with a watched pair after resolving its head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadUpdate {
    pub monitor_id: i64,
    pub repo_id: RepoId,
    pub rev: String,
    pub commit_oid: String,
}

/// Reads and writes the `cm_*` tables.
#[derive(Debug, Clone)]
pub struct MonitorStore {
    pool: PgPool,
    keyring: Option<Arc<Keyring>>,
}

impl MonitorStore {
    /// A store without encryption keys: webhook URLs are written in plaintext and encrypted
    /// ones cannot be read (see [`MonitorStore::with_keyring`]).
    pub fn new(db: &Db) -> Self {
        Self {
            pool: db.pool().clone(),
            keyring: None,
        }
    }

    /// Sets the keyring that encrypts and decrypts webhook URLs.
    #[must_use]
    pub fn with_keyring(mut self, keyring: Option<Arc<Keyring>>) -> Self {
        self.keyring = keyring;
        self
    }

    /// The stored form of an action's URL and header: `(url, url_enc)` and
    /// `(header, header_enc)`.
    fn secret_columns(&self, a: &ActionSpec) -> Result<StoredSecrets> {
        Ok(StoredSecrets {
            url: self.stored_form(&a.url)?,
            header: self.stored_form(&a.header)?,
        })
    }

    /// The stored form of a secret column: with a keyring a non-empty value is encrypted and
    /// the plaintext column left empty.
    fn stored_form(&self, value: &str) -> Result<(String, Option<Vec<u8>>)> {
        match &self.keyring {
            Some(k) if !value.is_empty() => Ok((
                String::new(),
                Some(k.encrypt(RecordClass::CodeMonitorWebhook, value.as_bytes())?),
            )),
            _ => Ok((value.to_owned(), None)),
        }
    }

    /// An action's URL or header from its stored form.
    fn read_secret(&self, plain: String, enc: Option<Vec<u8>>) -> Result<String> {
        let Some(blob) = enc else {
            return Ok(plain);
        };
        let keyring = self.keyring.as_deref().ok_or(DbError::Invalid(
            "encryption keys are not configured (set SOURCEGULL_KEYS_FILE or SOURCEGULL_KEYS); \
             the code monitor webhook URL or email header is encrypted",
        ))?;
        String::from_utf8(
            keyring
                .decrypt(RecordClass::CodeMonitorWebhook, &blob)?
                .to_vec(),
        )
        .map_err(|_| DbError::Invalid("an encrypted watch action secret is not UTF-8"))
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Whether the store has encryption keys (HTTP actions are then queued as channel jobs).
    pub fn has_keyring(&self) -> bool {
        self.keyring.is_some()
    }

    fn outbound(&self) -> OutboundWebhookStore {
        OutboundWebhookStore::new(&Db::from_pool(self.pool.clone()), self.keyring.clone())
    }

    /// Cancels the waiting channel jobs of the watch's actions that are off, or of all of
    /// them when the watch is off.
    async fn cancel_turned_off(tx: &mut Transaction<'_, Postgres>, monitor_id: i64) -> Result<()> {
        let off: Vec<i64> = sqlx::query_scalar(
            "SELECT a.id FROM cm_actions a JOIN cm_monitors m ON m.id = a.monitor_id
             WHERE m.id = $1 AND NOT (a.enabled AND m.enabled)",
        )
        .bind(monitor_id)
        .fetch_all(&mut **tx)
        .await?;
        OutboundWebhookStore::cancel_channel_jobs(tx, &off, TURNED_OFF).await?;
        Ok(())
    }

    /// The trigger job queue: three attempts, backing off from 30 s.
    pub fn trigger_queue_config() -> QueueConfig {
        let mut c = QueueConfig::new(TRIGGER_JOBS);
        c.backoff_base = std::time::Duration::from_secs(30);
        c
    }

    /// The action job queue: five attempts, backing off from 30 s (up to an hour).
    pub fn action_queue_config() -> QueueConfig {
        let mut c = QueueConfig::new(ACTION_JOBS);
        c.max_failures = 5;
        c.backoff_base = std::time::Duration::from_secs(30);
        c
    }

    /// Creates a watch with its actions.
    pub async fn create(&self, new: &NewMonitor) -> Result<Monitor> {
        let mut tx = self.pool.begin().await?;
        let (user, org) = namespace_columns(new.namespace);
        let row = sqlx::query(&format!(
            "INSERT INTO cm_monitors (created_by, updated_by, description, enabled,
                 namespace_user_id, namespace_org_id, query, unfiltered_by, unfiltered_at)
             VALUES ($1, $1, $2, $3, $4, $5, $6, CASE WHEN $7 THEN $1 END,
                 CASE WHEN $7 THEN now() END)
             RETURNING {MONITOR_COLUMNS}"
        ))
        .bind(new.created_by)
        .bind(&new.description)
        .bind(new.enabled)
        .bind(user)
        .bind(org)
        .bind(&new.query)
        .bind(new.unfiltered)
        .fetch_one(&mut *tx)
        .await?;
        let monitor = monitor_from_row(&row)?;
        for a in &new.actions {
            let secrets = self.secret_columns(a)?;
            insert_action(&mut tx, monitor.id, a, secrets).await?;
        }
        notify(&mut *tx, CHANNEL, &monitor.id.to_string()).await?;
        tx.commit().await?;
        Ok(monitor)
    }

    pub async fn get(&self, id: i64) -> Result<Option<Monitor>> {
        let row = sqlx::query(&format!(
            "SELECT {MONITOR_COLUMNS} FROM cm_monitors WHERE id = $1"
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.as_ref().map(monitor_from_row).transpose()?)
    }

    /// A page of watches, oldest first, and the total.
    pub async fn list(&self, opts: &ListMonitors) -> Result<(Vec<Monitor>, i64)> {
        let filter = "($1::bigint IS NULL OR namespace_user_id = $1
             OR namespace_org_id IN (SELECT org_id FROM org_members WHERE user_id = $1))";
        let rows = sqlx::query(&format!(
            "SELECT {MONITOR_COLUMNS} FROM cm_monitors WHERE {filter}
             ORDER BY id LIMIT $2 OFFSET $3"
        ))
        .bind(opts.affiliated_user)
        .bind(opts.limit)
        .bind(opts.offset)
        .fetch_all(&self.pool)
        .await?;
        let total: i64 =
            sqlx::query_scalar(&format!("SELECT count(*) FROM cm_monitors WHERE {filter}"))
                .bind(opts.affiliated_user)
                .fetch_one(&self.pool)
                .await?;
        let monitors = rows
            .iter()
            .map(monitor_from_row)
            .collect::<sqlx::Result<_>>()?;
        Ok((monitors, total))
    }

    /// Every enabled watch.
    pub async fn list_enabled(&self) -> Result<Vec<Monitor>> {
        let rows = sqlx::query(&format!(
            "SELECT {MONITOR_COLUMNS} FROM cm_monitors WHERE enabled ORDER BY id"
        ))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(monitor_from_row)
            .collect::<sqlx::Result<_>>()?)
    }

    /// Replaces a watch's fields and actions. A new query (or namespace, whose permissions
    /// it runs with) forgets the commits searched, so the next evaluation records the heads.
    pub async fn update(&self, id: i64, u: &MonitorUpdate) -> Result<Monitor> {
        let mut tx = self.pool.begin().await?;
        let old = sqlx::query(&format!(
            "SELECT {MONITOR_COLUMNS} FROM cm_monitors WHERE id = $1 FOR UPDATE"
        ))
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(DbError::NotFound("watch"))?;
        let old = monitor_from_row(&old)?;
        let rewatch =
            old.query != u.query || old.namespace != u.namespace || (u.enabled && !old.enabled);
        let (user, org) = namespace_columns(u.namespace);
        let row = sqlx::query(&format!(
            "UPDATE cm_monitors SET description = $2, enabled = $3, namespace_user_id = $4,
                 namespace_org_id = $5, query = $6, updated_by = $7, updated_at = now(),
                 query_updated_at = CASE WHEN query <> $6 THEN now() ELSE query_updated_at END,
                 unfiltered_by = CASE WHEN $8 THEN $7 END,
                 unfiltered_at = CASE WHEN NOT $8 THEN NULL
                     WHEN unfiltered_by = $7 THEN unfiltered_at ELSE now() END
             WHERE id = $1 RETURNING {MONITOR_COLUMNS}"
        ))
        .bind(id)
        .bind(&u.description)
        .bind(u.enabled)
        .bind(user)
        .bind(org)
        .bind(&u.query)
        .bind(u.updated_by)
        .bind(u.unfiltered)
        .fetch_one(&mut *tx)
        .await?;
        let monitor = monitor_from_row(&row)?;
        let keep: Vec<i64> = u.actions.iter().filter_map(|a| a.id).collect();
        let existing: Vec<i64> =
            sqlx::query_scalar("SELECT id FROM cm_actions WHERE monitor_id = $1")
                .bind(id)
                .fetch_all(&mut *tx)
                .await?;
        if keep.iter().any(|k| !existing.contains(k)) {
            return Err(DbError::NotFound("watch action"));
        }
        sqlx::query("DELETE FROM cm_actions WHERE monitor_id = $1 AND NOT (id = ANY($2))")
            .bind(id)
            .bind(&keep)
            .execute(&mut *tx)
            .await?;
        for a in &u.actions {
            let secrets = self.secret_columns(&a.spec)?;
            match a.id {
                Some(action_id) => update_action(&mut tx, action_id, &a.spec, secrets).await?,
                None => {
                    insert_action(&mut tx, id, &a.spec, secrets).await?;
                }
            }
        }
        if rewatch {
            sqlx::query("DELETE FROM cm_last_searched WHERE monitor_id = $1")
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }
        Self::cancel_turned_off(&mut tx, id).await?;
        notify(&mut *tx, CHANNEL, &id.to_string()).await?;
        tx.commit().await?;
        Ok(monitor)
    }

    /// Enables or disables a watch. Enabling forgets the commits searched, so what was
    /// pushed while it was off does not notify.
    pub async fn set_enabled(&self, id: i64, enabled: bool, by: UserId) -> Result<Monitor> {
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query(&format!(
            "UPDATE cm_monitors SET enabled = $2, updated_by = $3, updated_at = now(),
                 unfiltered_by = CASE WHEN unfiltered_by = $3 THEN unfiltered_by END,
                 unfiltered_at = CASE WHEN unfiltered_by = $3 THEN unfiltered_at END
             WHERE id = $1 RETURNING {MONITOR_COLUMNS}"
        ))
        .bind(id)
        .bind(enabled)
        .bind(by)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(DbError::NotFound("watch"))?;
        if enabled {
            sqlx::query("DELETE FROM cm_last_searched WHERE monitor_id = $1")
                .bind(id)
                .execute(&mut *tx)
                .await?;
        } else {
            Self::cancel_turned_off(&mut tx, id).await?;
        }
        notify(&mut *tx, CHANNEL, &id.to_string()).await?;
        tx.commit().await?;
        Ok(monitor_from_row(&row)?)
    }

    pub async fn delete(&self, id: i64) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let n = sqlx::query("DELETE FROM cm_monitors WHERE id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if n == 0 {
            return Err(DbError::NotFound("watch"));
        }
        notify(&mut *tx, CHANNEL, &id.to_string()).await?;
        tx.commit().await?;
        Ok(())
    }

    /// `resetTriggerQueryTimestamps`: the next evaluation searches the whole history of every
    /// watched revision.
    pub async fn reset(&self, id: i64) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let n = sqlx::query("UPDATE cm_monitors SET search_all = true WHERE id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if n == 0 {
            return Err(DbError::NotFound("watch"));
        }
        notify(&mut *tx, CHANNEL, &id.to_string()).await?;
        tx.commit().await?;
        Ok(())
    }

    /// The watch's actions, oldest first.
    pub async fn actions(&self, monitor_id: i64) -> Result<Vec<Action>> {
        let rows = sqlx::query(
            "SELECT id, monitor_id, kind, enabled, include_results, url, url_enc, chat_id, priority,
                 header, header_enc, created_at, updated_at
             FROM cm_actions WHERE monitor_id = $1 ORDER BY id",
        )
        .bind(monitor_id)
        .fetch_all(&self.pool)
        .await?;
        let mut out = Vec::with_capacity(rows.len());
        for r in &rows {
            out.push(self.action_from_row(r).await?);
        }
        Ok(out)
    }

    pub async fn action(&self, id: i64) -> Result<Option<Action>> {
        let row = sqlx::query(
            "SELECT id, monitor_id, kind, enabled, include_results, url, url_enc, chat_id, priority,
                 header, header_enc, created_at, updated_at
             FROM cm_actions WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        match row {
            Some(r) => Ok(Some(self.action_from_row(&r).await?)),
            None => Ok(None),
        }
    }

    async fn action_from_row(&self, r: &PgRow) -> Result<Action> {
        let id: i64 = r.try_get("id")?;
        let kind: String = r.try_get("kind")?;
        let priority: String = r.try_get("priority")?;
        let recipients = sqlx::query(
            "SELECT namespace_user_id, namespace_org_id FROM cm_recipients
             WHERE action_id = $1 ORDER BY id",
        )
        .bind(id)
        .fetch_all(&self.pool)
        .await?
        .iter()
        .filter_map(|r| {
            let user: Option<UserId> = r.try_get("namespace_user_id").ok()?;
            let org: Option<OrgId> = r.try_get("namespace_org_id").ok()?;
            user.map(Namespace::User).or(org.map(Namespace::Org))
        })
        .collect();
        Ok(Action {
            id,
            monitor_id: r.try_get("monitor_id")?,
            spec: ActionSpec {
                kind: ActionKind::parse(&kind).ok_or(DbError::Invalid("unknown action kind"))?,
                enabled: r.try_get("enabled")?,
                include_results: r.try_get("include_results")?,
                url: self.read_secret(r.try_get("url")?, r.try_get("url_enc")?)?,
                chat_id: r.try_get("chat_id")?,
                priority: if priority == "CRITICAL" {
                    Priority::Critical
                } else {
                    Priority::Normal
                },
                header: self.read_secret(r.try_get("header")?, r.try_get("header_enc")?)?,
                recipients,
            },
            created_at: r.try_get("created_at")?,
            updated_at: r.try_get("updated_at")?,
        })
    }

    /// The last searched commits of `monitors`.
    pub async fn last_searched(&self, monitors: &[i64]) -> Result<Vec<LastSearched>> {
        Ok(sqlx::query_as(
            "SELECT monitor_id, repo_id, rev, commit_oid, checked_at FROM cm_last_searched
             WHERE monitor_id = ANY($1)",
        )
        .bind(monitors)
        .fetch_all(&self.pool)
        .await?)
    }

    /// When each repository's refs last changed on its repo node.
    pub async fn repos_last_changed(&self, repos: &[i32]) -> Result<Vec<(RepoId, DateTime<Utc>)>> {
        Ok(sqlx::query_as(
            "SELECT repo_id, last_changed FROM gitserver_repos
             WHERE repo_id = ANY($1) AND last_changed IS NOT NULL",
        )
        .bind(repos)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Records evaluated heads in one transaction: `heads` become the last searched commits,
    /// `checked` pairs are marked checked at `now`, each `(monitor, query, ranges)` becomes a
    /// trigger job, and `searched_all` watches drop their `search_all` flag. Returns the new
    /// trigger job ids. Fails with [`DbError::Fenced`] (writing nothing) when `fence` is no
    /// longer current.
    pub async fn record_evaluation(
        &self,
        heads: &[HeadUpdate],
        checked: &[(i64, RepoId)],
        jobs: &[(i64, String, Vec<CommitRange>)],
        searched_all: &[i64],
        now: DateTime<Utc>,
        fence: &Fence,
    ) -> Result<Vec<i64>> {
        let mut tx = fence.begin(&self.pool).await?;
        for h in heads {
            // The watch may have been deleted meanwhile.
            sqlx::query(
                "INSERT INTO cm_last_searched (monitor_id, repo_id, rev, commit_oid, checked_at)
                 SELECT $1, $2, $3, $4, $5 WHERE EXISTS (SELECT 1 FROM cm_monitors WHERE id = $1)
                 ON CONFLICT (monitor_id, repo_id, rev)
                 DO UPDATE SET commit_oid = EXCLUDED.commit_oid, checked_at = EXCLUDED.checked_at",
            )
            .bind(h.monitor_id)
            .bind(h.repo_id)
            .bind(&h.rev)
            .bind(&h.commit_oid)
            .bind(now)
            .execute(&mut *tx)
            .await?;
        }
        if !checked.is_empty() {
            let monitors: Vec<i64> = checked.iter().map(|(m, _)| *m).collect();
            let repos: Vec<i32> = checked.iter().map(|(_, r)| r.get()).collect();
            sqlx::query(
                "UPDATE cm_last_searched l SET checked_at = $3
                 FROM unnest($1::bigint[], $2::integer[]) AS c(monitor_id, repo_id)
                 WHERE l.monitor_id = c.monitor_id AND l.repo_id = c.repo_id",
            )
            .bind(&monitors)
            .bind(&repos)
            .bind(now)
            .execute(&mut *tx)
            .await?;
        }
        let mut ids = Vec::with_capacity(jobs.len());
        for (monitor, query, ranges) in jobs {
            let id: Option<i64> = sqlx::query_scalar(
                "INSERT INTO cm_trigger_jobs (monitor_id, query, ranges)
                 SELECT $1, $2, $3 WHERE EXISTS (SELECT 1 FROM cm_monitors WHERE id = $1)
                 RETURNING id",
            )
            .bind(monitor)
            .bind(query)
            .bind(Json(ranges))
            .fetch_optional(&mut *tx)
            .await?;
            ids.extend(id);
        }
        if !searched_all.is_empty() {
            sqlx::query("UPDATE cm_monitors SET search_all = false WHERE id = ANY($1)")
                .bind(searched_all)
                .execute(&mut *tx)
                .await?;
        }
        if !ids.is_empty() {
            sourcegull_pgqueue::notify_enqueued(&mut *tx, TRIGGER_JOBS).await?;
        }
        tx.commit().await?;
        Ok(ids)
    }

    /// Stores a trigger job's results and, when there are any, queues each enabled action
    /// once per trigger job (if the job runs again, actions already queued are skipped):
    /// `channels` (rendered HTTP actions) as channel jobs, the other actions as action jobs.
    pub async fn finish_trigger_job(
        &self,
        job_id: i64,
        results: &[MonitorResult],
        channels: &[ChannelAction],
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let monitor: Option<i64> = sqlx::query_scalar(
            "UPDATE cm_trigger_jobs SET results = $2, num_results = $3 WHERE id = $1
             RETURNING monitor_id",
        )
        .bind(job_id)
        .bind(Json(results))
        .bind(i32::try_from(results.len()).unwrap_or(i32::MAX))
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(monitor) = monitor
            && !results.is_empty()
        {
            let action_jobs: Vec<i64> = sqlx::query_scalar(
                "SELECT action_id FROM cm_action_jobs WHERE trigger_job_id = $1",
            )
            .bind(job_id)
            .fetch_all(&mut *tx)
            .await?;
            let outbound = self.outbound();
            for c in channels
                .iter()
                .filter(|c| !action_jobs.contains(&c.action_id))
            {
                outbound
                    .enqueue_channel(
                        &mut tx,
                        &NewChannelJob {
                            channel: c.channel,
                            target: &c.target,
                            secret: None,
                            body: &c.body,
                            cm_action_id: Some(c.action_id),
                            cm_trigger_job_id: Some(job_id),
                        },
                    )
                    .await?;
            }
            let rendered: Vec<i64> = channels.iter().map(|c| c.action_id).collect();
            let n = sqlx::query(
                "INSERT INTO cm_action_jobs (action_id, trigger_job_id)
                 SELECT id, $2 FROM cm_actions a WHERE monitor_id = $1 AND enabled
                     AND NOT (id = ANY($3))
                     AND NOT EXISTS (SELECT 1 FROM outbound_webhook_jobs o
                         WHERE o.cm_action_id = a.id AND o.cm_trigger_job_id = $2)
                 ON CONFLICT (action_id, trigger_job_id) DO NOTHING",
            )
            .bind(monitor)
            .bind(job_id)
            .bind(&rendered)
            .execute(&mut *tx)
            .await?
            .rows_affected();
            if n > 0 {
                sourcegull_pgqueue::notify_enqueued(&mut *tx, ACTION_JOBS).await?;
            }
        }
        tx.commit().await?;
        Ok(())
    }

    /// A trigger job.
    pub async fn trigger_event(&self, id: i64) -> Result<Option<TriggerEvent>> {
        let row = sqlx::query(&format!(
            "SELECT {TRIGGER_COLUMNS} FROM cm_trigger_jobs WHERE id = $1"
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.as_ref().map(trigger_event_from_row).transpose()?)
    }

    /// A watch's trigger jobs, newest first, and the total.
    pub async fn trigger_events(
        &self,
        monitor_id: i64,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<TriggerEvent>, i64)> {
        let rows = sqlx::query(&format!(
            "SELECT {TRIGGER_COLUMNS} FROM cm_trigger_jobs WHERE monitor_id = $1
             ORDER BY id DESC LIMIT $2 OFFSET $3"
        ))
        .bind(monitor_id)
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;
        let total: i64 =
            sqlx::query_scalar("SELECT count(*) FROM cm_trigger_jobs WHERE monitor_id = $1")
                .bind(monitor_id)
                .fetch_one(&self.pool)
                .await?;
        let events = rows
            .iter()
            .map(trigger_event_from_row)
            .collect::<sqlx::Result<_>>()?;
        Ok((events, total))
    }

    /// Deliveries, newest first, and the total: of one action, optionally of one trigger
    /// job. They are the action jobs and the channel jobs of watch actions.
    pub async fn action_events(
        &self,
        action_id: Option<i64>,
        trigger_job_id: Option<i64>,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<ActionEvent>, i64)> {
        let union =
            "SELECT id, false AS channel_job, action_id, trigger_job_id, state, failure_message,
                 num_failures, queued_at, finished_at
             FROM cm_action_jobs
             WHERE ($1::bigint IS NULL OR action_id = $1)
               AND ($2::bigint IS NULL OR trigger_job_id = $2)
             UNION ALL
             SELECT id, true, cm_action_id, cm_trigger_job_id, state, failure_message,
                 num_failures, queued_at, finished_at
             FROM outbound_webhook_jobs
             WHERE cm_action_id IS NOT NULL AND cm_trigger_job_id IS NOT NULL
               AND ($1::bigint IS NULL OR cm_action_id = $1)
               AND ($2::bigint IS NULL OR cm_trigger_job_id = $2)";
        let events = sqlx::query_as(&format!(
            "SELECT * FROM ({union}) e ORDER BY queued_at DESC, channel_job DESC, id DESC
             LIMIT $3 OFFSET $4"
        ))
        .bind(action_id)
        .bind(trigger_job_id)
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;
        let total: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM ({union}) e"))
            .bind(action_id)
            .bind(trigger_job_id)
            .fetch_one(&self.pool)
            .await?;
        Ok((events, total))
    }

    /// The id of the last `repo_ref_events` row the evaluator consumed.
    pub async fn cursor(&self) -> Result<i64> {
        Ok(
            sqlx::query_scalar("SELECT last_event_id FROM cm_ref_events_cursor")
                .fetch_optional(&self.pool)
                .await?
                .unwrap_or(0),
        )
    }

    /// Moves the evaluator's cursor to `id` while `fence` is current.
    pub async fn set_cursor(&self, id: i64, fence: &Fence) -> Result<()> {
        let mut tx = fence.begin(&self.pool).await?;
        sqlx::query(
            "INSERT INTO cm_ref_events_cursor (id, last_event_id) VALUES (true, $1)
             ON CONFLICT (id) DO UPDATE SET last_event_id = EXCLUDED.last_event_id",
        )
        .bind(id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Deletes finished trigger jobs (and their action and channel jobs) older than `before`.
    pub async fn delete_finished_before(&self, before: DateTime<Utc>) -> Result<u64> {
        Ok(sqlx::query(
            "DELETE FROM cm_trigger_jobs WHERE finished_at < $1
             AND NOT EXISTS (SELECT 1 FROM cm_action_jobs a WHERE a.trigger_job_id = cm_trigger_jobs.id
                 AND a.state IN ('queued', 'processing', 'errored'))
             AND NOT EXISTS (SELECT 1 FROM outbound_webhook_jobs o
                 WHERE o.cm_trigger_job_id = cm_trigger_jobs.id
                 AND o.state IN ('queued', 'processing', 'errored'))",
        )
        .bind(before)
        .execute(&self.pool)
        .await?
        .rows_affected())
    }

    /// The email addresses of `ns`: a user's primary (else first) verified address, or those
    /// of every member of an org. Deactivated users get none.
    pub async fn recipient_emails(&self, ns: Namespace) -> Result<Vec<String>> {
        Ok(self
            .recipient_users(ns)
            .await?
            .into_iter()
            .map(|(_, email)| email)
            .collect())
    }

    /// The users of `ns` (the user, or every member of an org) with their primary (else
    /// first) verified address. Deactivated users and users without one are left out.
    pub async fn recipient_users(&self, ns: Namespace) -> Result<Vec<(UserId, String)>> {
        let users: Vec<UserId> = match ns {
            Namespace::User(u) => vec![u],
            Namespace::Org(o) => {
                sqlx::query_scalar("SELECT user_id FROM org_members WHERE org_id = $1")
                    .bind(o)
                    .fetch_all(&self.pool)
                    .await?
            }
        };
        Ok(sqlx::query_as(
            "SELECT DISTINCT ON (e.user_id) e.user_id, e.email FROM user_emails e
             JOIN users u ON u.id = e.user_id AND u.deleted_at IS NULL
             WHERE e.user_id = ANY($1) AND e.verified_at IS NOT NULL
             ORDER BY e.user_id, e.is_primary DESC, e.created_at",
        )
        .bind(&users)
        .fetch_all(&self.pool)
        .await?)
    }
}

/// The stored forms of an action's secret columns, from [`MonitorStore::secret_columns`].
struct StoredSecrets {
    url: (String, Option<Vec<u8>>),
    header: (String, Option<Vec<u8>>),
}

async fn insert_action(
    tx: &mut Transaction<'_, Postgres>,
    monitor_id: i64,
    a: &ActionSpec,
    secrets: StoredSecrets,
) -> Result<i64> {
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO cm_actions (monitor_id, kind, enabled, include_results, url, priority, header,
             url_enc, chat_id, header_enc)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) RETURNING id",
    )
    .bind(monitor_id)
    .bind(a.kind.as_str())
    .bind(a.enabled)
    .bind(a.include_results)
    .bind(secrets.url.0)
    .bind(a.priority.as_str())
    .bind(secrets.header.0)
    .bind(secrets.url.1)
    .bind(&a.chat_id)
    .bind(secrets.header.1)
    .fetch_one(&mut **tx)
    .await?;
    insert_recipients(tx, id, &a.recipients).await?;
    Ok(id)
}

async fn update_action(
    tx: &mut Transaction<'_, Postgres>,
    id: i64,
    a: &ActionSpec,
    secrets: StoredSecrets,
) -> Result<()> {
    let n = sqlx::query(
        "UPDATE cm_actions SET enabled = $3, include_results = $4, url = $5, url_enc = $8,
             priority = $6, header = $7, chat_id = $9, header_enc = $10, updated_at = now()
         WHERE id = $1 AND kind = $2",
    )
    .bind(id)
    .bind(a.kind.as_str())
    .bind(a.enabled)
    .bind(a.include_results)
    .bind(secrets.url.0)
    .bind(a.priority.as_str())
    .bind(secrets.header.0)
    .bind(secrets.url.1)
    .bind(&a.chat_id)
    .bind(secrets.header.1)
    .execute(&mut **tx)
    .await?
    .rows_affected();
    if n == 0 {
        return Err(DbError::NotFound("watch action"));
    }
    sqlx::query("DELETE FROM cm_recipients WHERE action_id = $1")
        .bind(id)
        .execute(&mut **tx)
        .await?;
    insert_recipients(tx, id, &a.recipients).await
}

async fn insert_recipients(
    tx: &mut Transaction<'_, Postgres>,
    action_id: i64,
    recipients: &[Namespace],
) -> Result<()> {
    for r in recipients {
        let (user, org) = namespace_columns(*r);
        sqlx::query(
            "INSERT INTO cm_recipients (action_id, namespace_user_id, namespace_org_id)
             VALUES ($1, $2, $3)",
        )
        .bind(action_id)
        .bind(user)
        .bind(org)
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}
