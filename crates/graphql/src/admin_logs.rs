//! Background jobs and admin logs, site admins only:
//!
//! - `backgroundJobs` (`BackgroundJob`, `BackgroundRoutine` with instances, recent runs and
//!   stats). Two jobs: the leader-elected routines and the `pgqueue` queues. A
//!   queue routine carries `queue` (counts by state, oldest queued job,
//!   recent failures), and `retryBackgroundQueueJob` / `cancelBackgroundQueueJob` act on one
//!   job.
//! - `outboundRequests` (`OutboundRequest`): code host API calls, redacted
//!   (`outboundRequestLogLimit`).
//! - `slowRequests` (`SlowRequest`): GraphQL requests and searches above
//!   `observability.logSlowGraphQLRequests` / `observability.logSlowSearches`. A GraphQL
//!   request's variables are stored as [`slow_request_variables`] returns them.
//! - `webhookLogs` and `ExternalService.webhookLogs` (`WebhookLog`): incoming code host
//!   webhook deliveries.

use async_graphql::{Context, Enum, ID, Object, Result, SimpleObject};
use chrono::{DateTime, Utc};
use sourcegull_db::{
    OutboundRequestLogItem, RoutineSnapshot, SlowRequestKind, SlowRequestLogItem, WebhookLog,
    WebhookLogFilter,
};
use sourcegull_outbound_webhooks::store::{LoggedRequest, LoggedResponse};
use sourcegull_services::relay::{marshal_id, marshal_string_id, unmarshal_id, unmarshal_int_id};
use sourcegull_types::{ExternalServiceId, UserId};

use crate::EmptyResponse;
use crate::context::{codes, db_error, error, page_size, require_site_admin, services, to_int};
use crate::external_services::ExternalServiceNode;
use crate::pagination::{PageInfo, decode_cursor};
use crate::repos::RepositoryNode;
use crate::users::UserNode;
use sourcegull_outbound_webhooks_graphql::{
    HttpHeader, WebhookLogRequestNode, WebhookLogResponseNode,
};

/// Node kind of background job IDs.
pub const BACKGROUND_JOB_KIND: &str = "BackgroundJob";
/// Node kind of webhook log IDs.
pub const WEBHOOK_LOG_KIND: &str = "WebhookLog";
/// Node kind of outbound request IDs.
pub const OUTBOUND_REQUEST_KIND: &str = "OutboundRequest";
/// Cursor kind of id-based pages (the id of the last item).
const ID_CURSOR_KIND: &str = "IdCursor";

const MAX_PAGE: i64 = 1000;
/// Recent runs per routine when `recentRunCount` is omitted.
const DEFAULT_RECENT_RUNS: i32 = 5;
/// Recent failures and active jobs listed per queue.
const QUEUE_FAILURES: i64 = 5;

/// The job holding the leader-elected routines.
pub const ROUTINES_JOB: &str = "worker: leader-elected routines";
/// The job holding the queues.
pub const QUEUES_JOB: &str = "worker: job queues";

/// A known leader-elected routine.
struct RoutineInfo {
    name: &'static str,
    description: &'static str,
    interval_ms: Option<i32>,
}

/// Every leader-elected routine of the worker role, by lease name.
const ROUTINES: &[RoutineInfo] = &[
    RoutineInfo {
        name: "session-janitor",
        description: "Deletes expired sessions and OAuth sign-in flows.",
        interval_ms: Some(600_000),
    },
    RoutineInfo {
        name: "repo-sync-scheduler",
        description: "Queues a discovery sync for each code host connection whose \
            repoListUpdateInterval elapsed; prunes webhook logs, ref events and job history.",
        interval_ms: Some(10_000),
    },
    RoutineInfo {
        name: "perms-sync-scheduler",
        description: "Queues permissions syncs of the users and repositories synced longest \
            ago (permissions.syncScheduleInterval) and prunes the sync history.",
        interval_ms: None,
    },
    RoutineInfo {
        name: "codeintel-upload-janitor",
        description: "Marks abandoned and expired SCIP uploads, deletes them with their \
            code graph files and removes orphaned blobs.",
        interval_ms: Some(300_000),
    },
    RoutineInfo {
        name: "codeintel-upload-expirer",
        description: "Applies the code graph data retention policies to uploads.",
        interval_ms: None,
    },
    RoutineInfo {
        name: "codeintel-ranker",
        description: "Ranks files by PageRank over the code graph of the uploads serving \
            default branch heads.",
        interval_ms: Some(600_000),
    },
    RoutineInfo {
        name: "codeintel-autoindex-scheduler",
        description: "Queues auto-indexing jobs on ref updates and in a periodic sweep.",
        interval_ms: Some(120_000),
    },
    RoutineInfo {
        name: "search-jobs-janitor",
        description: "Deletes search jobs past searchJobs.retention with their results.",
        interval_ms: Some(3_600_000),
    },
    RoutineInfo {
        name: "outbound-webhook-janitor",
        description: "Deletes outbound webhook delivery logs past \
            webhooks.outbound.logRetention.",
        interval_ms: Some(3_600_000),
    },
    RoutineInfo {
        name: "oauth-janitor",
        description: "Deletes expired OAuth authorization requests and codes, and MCP clients \
            that are not used.",
        interval_ms: Some(3_600_000),
    },
    RoutineInfo {
        name: "batches-changesets-retention",
        description: "Deletes changesets past batchChanges.changesetsRetention.",
        interval_ms: Some(3_600_000),
    },
    RoutineInfo {
        name: "batches-previews-janitor",
        description: "Deletes bulk change specs never applied after 7 days and changeset specs \
            never attached after 2 days.",
        interval_ms: Some(3_600_000),
    },
    RoutineInfo {
        name: "batches-changeset-syncer",
        description: "Refreshes the state of published changesets from the code host.",
        interval_ms: Some(30_000),
    },
    RoutineInfo {
        name: "batches-changeset-scheduler",
        description: "Publishes changesets within batchChanges.rolloutWindows.",
        interval_ms: Some(10_000),
    },
    RoutineInfo {
        name: "code-monitors-evaluator",
        description: "Queues watch searches on ref updates and in a sweep.",
        interval_ms: Some(60_000),
    },
    RoutineInfo {
        name: "insights-enqueuer",
        description: "Starts trend backfills and queues recordings.",
        interval_ms: Some(5_000),
    },
    RoutineInfo {
        name: "insights-janitor",
        description: "Purges deleted trend series, old points and finished jobs.",
        interval_ms: Some(3_600_000),
    },
    RoutineInfo {
        name: "own-signals-scheduler",
        description: "Queues the Code Ownership signal indexers on ref updates and daily.",
        interval_ms: None,
    },
    RoutineInfo {
        name: "oob-migration-runner",
        description: "Runs out-of-band migrations in batches.",
        interval_ms: Some(30_000),
    },
];

/// Descriptions of known queues; queues are discovered by shape, so new ones show up
/// with an empty description.
const QUEUES: &[(&str, &str)] = &[
    (
        "external_service_sync_jobs",
        "Discovery syncs of code host connections.",
    ),
    (
        "repo_update_jobs",
        "Clones and fetches of repositories (run by the repository nodes).",
    ),
    (
        "permission_sync_jobs",
        "Permissions syncs of users and repositories.",
    ),
    (
        "codeintel_uploads",
        "SCIP upload processing into code graph files.",
    ),
    (
        "codeintel_index_jobs",
        "Auto-indexing jobs (taken by executors).",
    ),
    (
        "batch_spec_resolution_jobs",
        "Bulk change spec workspace resolution.",
    ),
    (
        "batch_spec_workspace_execution_jobs",
        "Bulk change spec workspace executions (taken by executors).",
    ),
    ("changesets", "The bulk changes reconciler."),
    ("changeset_jobs", "Bulk operations on changesets."),
    (
        "exhaustive_search_jobs",
        "Search jobs: splitting into repositories.",
    ),
    (
        "exhaustive_search_repo_jobs",
        "Search jobs: splitting repositories into revisions.",
    ),
    (
        "exhaustive_search_repo_revision_jobs",
        "Search jobs: searching one revision.",
    ),
    (
        "outbound_webhook_jobs",
        "Outbound webhook deliveries, and watch webhook, Slack, Mattermost and Telegram messages.",
    ),
    ("cm_trigger_jobs", "Watch searches over pushed commits."),
    (
        "cm_action_jobs",
        "Watch email deliveries, and other actions without encryption keys.",
    ),
    (
        "insight_backfill_jobs",
        "Trend backfills per series and repository.",
    ),
    (
        "insight_recording_jobs",
        "Trend recordings per series and point.",
    ),
    (
        "own_background_jobs",
        "Code Ownership signal indexers and analytics.",
    ),
];

fn id_cursor(id: i64) -> String {
    marshal_id(ID_CURSOR_KIND, id)
}

fn parse_id_cursor(after: Option<&str>) -> Result<Option<i64>> {
    match after {
        None | Some("") => Ok(None),
        Some(c) => unmarshal_int_id(ID_CURSOR_KIND, c)
            .map(Some)
            .ok_or_else(|| error(codes::INVALID_INPUT, format!("Invalid cursor {c:?}."))),
    }
}

/// Page info for id-paged lists: `fetched` holds up to `limit + 1` items.
fn id_page_info(limit: i64, fetched: usize, last_id: Option<i64>) -> PageInfo {
    let has_next_page = i64::try_from(fetched).unwrap_or(i64::MAX) > limit;
    PageInfo {
        has_next_page,
        end_cursor: if has_next_page {
            last_id.map(id_cursor)
        } else {
            None
        },
    }
}

fn duration_ms(ms: i64) -> i32 {
    i32::try_from(ms).unwrap_or(i32::MAX)
}

// ---------------------------------------------------------------------------------------------
// Background jobs

/// What kind of routine it is.
#[derive(Enum, Copy, Clone, Debug, PartialEq, Eq)]
pub enum BackgroundRoutineType {
    /// Periodic routine.
    Periodic,
    /// Periodic routine with metrics set up.
    PeriodicWithMetrics,
    /// DB-backed worker.
    DbBacked,
    /// Custom routine.
    Custom,
}

/// One process running (or that ran) a routine.
#[derive(SimpleObject)]
pub struct BackgroundRoutineInstance {
    /// The process' host name.
    host_name: String,
    /// When it last took the routine's lease; null when unknown.
    last_started_at: Option<DateTime<Utc>>,
    /// When it last gave the lease up; null when unknown.
    last_stopped_at: Option<DateTime<Utc>>,
}

/// One run of a routine (an execution of its handler, not a start or stop).
#[derive(SimpleObject)]
pub struct BackgroundRoutineRecentRun {
    /// When the run started.
    at: DateTime<Utc>,
    /// The host that ran it.
    host_name: String,
    duration_ms: i32,
    /// Why it failed, if it did.
    error_message: Option<String>,
}

/// Statistics of a routine's runs over the last seven days.
#[derive(SimpleObject, Default)]
pub struct BackgroundRoutineStats {
    /// The start of the earliest day with runs.
    since: Option<DateTime<Utc>>,
    run_count: i32,
    error_count: i32,
    min_duration_ms: i32,
    avg_duration_ms: i32,
    max_duration_ms: i32,
}

/// A job of a queue.
#[derive(SimpleObject)]
pub struct BackgroundQueueJob {
    /// The job's row id in its queue table.
    id: ID,
    /// `queued`, `processing`, `failed` or `errored`.
    state: String,
    failure_message: Option<String>,
    queued_at: DateTime<Utc>,
    started_at: Option<DateTime<Utc>>,
    finished_at: Option<DateTime<Utc>>,
    num_failures: i32,
    num_resets: i32,
    /// The worker that ran it last.
    worker_hostname: String,
}

/// A `pgqueue` queue.
#[derive(SimpleObject)]
pub struct BackgroundQueue {
    /// The queue table.
    table: String,
    queued: i32,
    processing: i32,
    completed: i32,
    errored: i32,
    failed: i32,
    canceled: i32,
    /// When the oldest job still queued was queued.
    oldest_queued_at: Option<DateTime<Utc>>,
    /// The newest failed or errored jobs, newest first.
    recent_failures: Vec<BackgroundQueueJob>,
    /// The longest-waiting queued and processing jobs, oldest first.
    active_jobs: Vec<BackgroundQueueJob>,
}

impl From<sourcegull_pgqueue::admin::QueueJob> for BackgroundQueueJob {
    fn from(f: sourcegull_pgqueue::admin::QueueJob) -> Self {
        Self {
            id: ID(f.id.to_string()),
            state: f.state,
            failure_message: f.failure_message,
            queued_at: f.queued_at,
            started_at: f.started_at,
            finished_at: f.finished_at,
            num_failures: f.num_failures,
            num_resets: f.num_resets,
            worker_hostname: f.worker_hostname,
        }
    }
}

/// A routine running inside a background job.
#[derive(SimpleObject)]
pub struct BackgroundRoutine {
    name: String,
    /// Periodic (leader-elected routines) or DB-backed (queues).
    #[graphql(name = "type")]
    kind: BackgroundRoutineType,
    description: String,
    /// The interval between runs, for periodic routines with a fixed one.
    interval_ms: Option<i32>,
    /// The processes that ran it (for routines, the lease holders).
    instances: Vec<BackgroundRoutineInstance>,
    /// The newest runs, newest first.
    recent_runs: Vec<BackgroundRoutineRecentRun>,
    /// Run statistics of the last seven days.
    stats: BackgroundRoutineStats,
    /// The queue, for DB-backed routines.
    queue: Option<BackgroundQueue>,
}

/// A background job: a group of routines.
pub struct BackgroundJobNode {
    name: String,
    routines: Vec<BackgroundRoutine>,
}

#[Object(name = "BackgroundJob")]
impl BackgroundJobNode {
    /// The opaque node ID.
    pub async fn id(&self) -> ID {
        ID(marshal_string_id(BACKGROUND_JOB_KIND, &self.name))
    }

    /// The name of the job.
    async fn name(&self) -> &str {
        &self.name
    }

    /// The routines that run inside this job.
    async fn routines(&self) -> &[BackgroundRoutine] {
        &self.routines
    }
}

/// A page of background jobs.
#[derive(SimpleObject)]
pub struct BackgroundJobConnection {
    nodes: Vec<BackgroundJobNode>,
    total_count: i32,
    page_info: PageInfo,
}

fn routine_from(
    info: Option<&RoutineInfo>,
    name: String,
    snap: Option<RoutineSnapshot>,
    recent_runs: usize,
) -> BackgroundRoutine {
    let snap = snap.unwrap_or_default();
    let s = snap.stats;
    BackgroundRoutine {
        name,
        kind: BackgroundRoutineType::Periodic,
        description: info.map(|i| i.description.to_owned()).unwrap_or_default(),
        interval_ms: info.and_then(|i| i.interval_ms),
        instances: snap
            .instances
            .into_iter()
            .map(|i| BackgroundRoutineInstance {
                host_name: i.host_name,
                last_started_at: i.last_started_at,
                last_stopped_at: i.last_stopped_at,
            })
            .collect(),
        recent_runs: snap
            .recent_runs
            .into_iter()
            .take(recent_runs)
            .map(|r| BackgroundRoutineRecentRun {
                at: r.at,
                host_name: r.host_name,
                duration_ms: duration_ms(r.duration_ms),
                error_message: r.error,
            })
            .collect(),
        stats: BackgroundRoutineStats {
            since: s.since,
            run_count: to_int(s.run_count),
            error_count: to_int(s.error_count),
            min_duration_ms: duration_ms(s.min_duration_ms),
            avg_duration_ms: duration_ms(s.avg_duration_ms),
            max_duration_ms: duration_ms(s.max_duration_ms),
        },
        queue: None,
    }
}

fn queue_error(e: sourcegull_pgqueue::Error) -> async_graphql::Error {
    db_error(sourcegull_db::DbError::Queue(e))
}

/// Both background jobs.
async fn background_jobs(ctx: &Context<'_>, recent_runs: usize) -> Result<Vec<BackgroundJobNode>> {
    let svc = services(ctx)?;
    let mut snapshots: std::collections::BTreeMap<String, RoutineSnapshot> =
        sourcegull_db::global_recorder()
            .snapshot(recent_runs)
            .await
            .map_err(db_error)?
            .into_iter()
            .map(|s| (s.name.clone(), s))
            .collect();
    let mut routines: Vec<BackgroundRoutine> = ROUTINES
        .iter()
        .map(|info| {
            let snap = snapshots.remove(info.name);
            routine_from(Some(info), info.name.to_owned(), snap, recent_runs)
        })
        .collect();
    // Routines recorded but not listed above (e.g. added without a description).
    routines.extend(
        snapshots
            .into_iter()
            .map(|(name, snap)| routine_from(None, name, Some(snap), recent_runs)),
    );

    let pool = svc.db.pool();
    let mut queues = Vec::new();
    for table in sourcegull_pgqueue::admin::queue_tables(pool)
        .await
        .map_err(queue_error)?
    {
        let s = sourcegull_pgqueue::admin::summarize(pool, &table, QUEUE_FAILURES)
            .await
            .map_err(queue_error)?;
        let description = QUEUES
            .iter()
            .find(|(t, _)| *t == table)
            .map(|(_, d)| (*d).to_owned())
            .unwrap_or_default();
        queues.push(BackgroundRoutine {
            name: table.clone(),
            kind: BackgroundRoutineType::DbBacked,
            description,
            interval_ms: None,
            instances: Vec::new(),
            recent_runs: Vec::new(),
            stats: BackgroundRoutineStats::default(),
            queue: Some(BackgroundQueue {
                table,
                queued: to_int(s.queued),
                processing: to_int(s.processing),
                completed: to_int(s.completed),
                errored: to_int(s.errored),
                failed: to_int(s.failed),
                canceled: to_int(s.canceled),
                oldest_queued_at: s.oldest_queued_at,
                recent_failures: s.recent_failures.into_iter().map(Into::into).collect(),
                active_jobs: s.active.into_iter().map(Into::into).collect(),
            }),
        });
    }
    Ok(vec![
        BackgroundJobNode {
            name: ROUTINES_JOB.to_owned(),
            routines,
        },
        BackgroundJobNode {
            name: QUEUES_JOB.to_owned(),
            routines: queues,
        },
    ])
}

/// Checks that `queue` names a queue table and parses a job id.
async fn queue_job(ctx: &Context<'_>, queue: &str, id: &ID) -> Result<i64> {
    let tables = sourcegull_pgqueue::admin::queue_tables(services(ctx)?.db.pool())
        .await
        .map_err(queue_error)?;
    if !tables.iter().any(|t| t == queue) {
        return Err(error(codes::NOT_FOUND, format!("No queue {queue:?}.")));
    }
    id.parse::<i64>()
        .map_err(|_| error(codes::INVALID_INPUT, format!("Invalid job id {:?}.", id.0)))
}

// ---------------------------------------------------------------------------------------------
// Outbound requests

fn header_list(v: &serde_json::Value) -> Vec<HttpHeader> {
    v.as_object()
        .map(|m| {
            m.iter()
                .map(|(name, values)| HttpHeader {
                    name: name.clone(),
                    values: match values {
                        serde_json::Value::Array(a) => a
                            .iter()
                            .map(|x| x.as_str().map_or_else(|| x.to_string(), str::to_owned))
                            .collect(),
                        serde_json::Value::String(s) => vec![s.clone()],
                        other => vec![other.to_string()],
                    },
                })
                .collect()
        })
        .unwrap_or_default()
}

/// One logged outbound request (secrets redacted).
pub struct OutboundRequestNode(OutboundRequestLogItem);

#[Object(name = "OutboundRequest")]
impl OutboundRequestNode {
    /// The request log item ID.
    pub async fn id(&self) -> ID {
        ID(marshal_id(OUTBOUND_REQUEST_KIND, self.0.id))
    }

    /// When the request was sent.
    async fn started_at(&self) -> DateTime<Utc> {
        self.0.started_at
    }

    async fn method(&self) -> &str {
        &self.0.method
    }

    /// The URL, with credentials redacted.
    async fn url(&self) -> &str {
        &self.0.url
    }

    /// The request headers, with auth headers redacted.
    async fn request_headers(&self) -> Vec<HttpHeader> {
        header_list(&self.0.request_headers)
    }

    /// The request body (truncated).
    async fn request_body(&self) -> &str {
        &self.0.request_body
    }

    /// The response status; 0 when no response arrived.
    async fn status_code(&self) -> i32 {
        self.0.status_code
    }

    async fn response_headers(&self) -> Vec<HttpHeader> {
        header_list(&self.0.response_headers)
    }

    async fn duration_ms(&self) -> i32 {
        duration_ms(self.0.duration_ms)
    }

    /// The transport error, empty when a response arrived.
    async fn error_message(&self) -> &str {
        &self.0.error_message
    }

    /// The client that sent the request (e.g. `gitlab https://gitlab.example.com/`).
    async fn creation_stack_frame(&self) -> &str {
        &self.0.creation_stack_frame
    }

    /// Where in Sourcegull the request was made, when known.
    async fn call_stack(&self) -> &str {
        &self.0.call_stack
    }
}

/// A page of outbound requests.
#[derive(SimpleObject)]
pub struct OutboundRequestConnection {
    nodes: Vec<OutboundRequestNode>,
    total_count: i32,
    page_info: PageInfo,
}

// ---------------------------------------------------------------------------------------------
// Slow requests

/// A logged slow GraphQL request or search.
pub struct SlowRequestNode(SlowRequestLogItem);

#[Object(name = "SlowRequest")]
impl SlowRequestNode {
    /// The index of this request (opaque).
    async fn index(&self) -> String {
        self.0.id.to_string()
    }

    /// When the request started.
    async fn start(&self) -> DateTime<Utc> {
        self.0.started_at
    }

    /// The duration of the request, in seconds.
    async fn duration(&self) -> f64 {
        self.0.duration_ms / 1000.0
    }

    /// The user who sent it, if signed in.
    async fn user(&self, ctx: &Context<'_>) -> Result<Option<UserNode>> {
        let Some(id) = self.0.user_id else {
            return Ok(None);
        };
        Ok(services(ctx)?
            .db
            .users()
            .get_by_id(UserId(id))
            .await
            .map_err(db_error)?
            .map(UserNode))
    }

    /// The GraphQL operation name, or `search`.
    async fn name(&self) -> &str {
        &self.0.name
    }

    /// Where it came from: `browser`, `other` or (searches) `stream`.
    async fn source(&self) -> &str {
        &self.0.source
    }

    /// The repository the request was about. Not recorded: always null.
    async fn repository(&self) -> Option<RepositoryNode> {
        None
    }

    /// The GraphQL variables as JSON (for searches, the pattern type).
    async fn variables(&self) -> &str {
        &self.0.variables
    }

    /// The errors the request returned.
    async fn errors(&self) -> Vec<String> {
        self.0
            .errors
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|e| e.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The GraphQL document or the search query.
    async fn query(&self) -> &str {
        &self.0.query
    }

    /// The file path the request was about. Not recorded: always null.
    async fn filepath(&self) -> Option<&str> {
        self.0.filepath.as_deref()
    }

    /// `GRAPHQL` or `SEARCH`.
    async fn kind(&self) -> SlowRequestKindEnum {
        if self.0.kind == SlowRequestKind::Search.as_str() {
            SlowRequestKindEnum::Search
        } else {
            SlowRequestKindEnum::Graphql
        }
    }
}

/// What kind of request was slow.
#[derive(Enum, Copy, Clone, Debug, PartialEq, Eq)]
#[graphql(name = "SlowRequestKind")]
pub enum SlowRequestKindEnum {
    /// A GraphQL request.
    Graphql,
    /// A search.
    Search,
}

/// A page of slow requests.
#[derive(SimpleObject)]
pub struct SlowRequestConnection {
    nodes: Vec<SlowRequestNode>,
    total_count: i32,
    page_info: PageInfo,
}

// ---------------------------------------------------------------------------------------------
// Webhook logs

/// One incoming code host webhook delivery.
pub struct WebhookLogNode(pub WebhookLog);

#[Object(name = "WebhookLog")]
impl WebhookLogNode {
    /// The webhook log ID.
    pub async fn id(&self) -> ID {
        ID(marshal_id(WEBHOOK_LOG_KIND, self.0.id))
    }

    /// When the delivery arrived.
    async fn received_at(&self) -> DateTime<Utc> {
        self.0.received_at
    }

    /// The code host connection the delivery was matched to, if any.
    async fn external_service(&self, ctx: &Context<'_>) -> Result<Option<ExternalServiceNode>> {
        let Some(id) = self.0.external_service_id else {
            return Ok(None);
        };
        let svc = services(ctx)?;
        Ok(svc
            .db
            .external_services(svc.keyring.clone())
            .get(id)
            .await
            .map_err(db_error)?
            .map(ExternalServiceNode::new))
    }

    /// The status the webhook handler answered with.
    async fn status_code(&self) -> i32 {
        self.0.status_code
    }

    /// The delivery: selected headers (never the secret) and the body.
    async fn request(&self) -> WebhookLogRequestNode {
        let r = &self.0.request;
        let mut headers = std::collections::BTreeMap::new();
        for h in header_list(&r["headers"]) {
            headers.insert(h.name, h.values);
        }
        let body = match &r["body"] {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Null => String::new(),
            other => serde_json::to_string_pretty(other).unwrap_or_default(),
        };
        WebhookLogRequestNode(LoggedRequest {
            method: r["method"].as_str().unwrap_or("POST").to_owned(),
            url: r["url"].as_str().unwrap_or_default().to_owned(),
            version: r["version"].as_str().unwrap_or("HTTP/1.1").to_owned(),
            headers,
            body,
        })
    }

    /// The answer: the status code and, on errors, the message.
    async fn response(&self) -> WebhookLogResponseNode {
        WebhookLogResponseNode(LoggedResponse {
            status_code: self.0.status_code,
            version: "HTTP/1.1".to_owned(),
            headers: std::collections::BTreeMap::new(),
            body: self.0.error.clone().unwrap_or_default(),
        })
    }

    /// The `X-Gitlab-Event` header, when sent.
    async fn event(&self) -> Option<&str> {
        self.0.event.as_deref()
    }
}

/// A page of webhook deliveries.
#[derive(SimpleObject)]
pub struct WebhookLogConnection {
    nodes: Vec<WebhookLogNode>,
    total_count: i32,
    page_info: PageInfo,
}

/// Webhook deliveries matching `filter`, newest first (site admins only).
pub async fn webhook_logs(
    ctx: &Context<'_>,
    mut filter: WebhookLogFilter,
    first: Option<i32>,
    after: Option<String>,
) -> Result<WebhookLogConnection> {
    require_site_admin(ctx)?;
    let limit = page_size(first, 50, MAX_PAGE);
    filter.before_id = parse_id_cursor(after.as_deref())?;
    let store = services(ctx)?.db.webhook_logs();
    let mut rows = store.search(&filter, limit + 1).await.map_err(db_error)?;
    let total = store.count(&filter).await.map_err(db_error)?;
    let fetched = rows.len();
    rows.truncate(usize::try_from(limit).unwrap_or(0));
    Ok(WebhookLogConnection {
        page_info: id_page_info(limit, fetched, rows.last().map(|r| r.id)),
        nodes: rows.into_iter().map(WebhookLogNode).collect(),
        total_count: to_int(total),
    })
}

/// A webhook log, outbound request or background job by ID for `node(id:)` (site admins only):
/// `None` when `kind` is not one of them.
pub async fn load_node(
    ctx: &Context<'_>,
    kind: &str,
    id: &str,
) -> Result<Option<Option<crate::node::Node>>> {
    use crate::node::Node;
    match kind {
        WEBHOOK_LOG_KIND | OUTBOUND_REQUEST_KIND | BACKGROUND_JOB_KIND => {}
        _ => return Ok(None),
    }
    require_site_admin(ctx)?;
    let db = &services(ctx)?.db;
    Ok(Some(match kind {
        WEBHOOK_LOG_KIND => match unmarshal_int_id(kind, id) {
            Some(key) => db
                .webhook_logs()
                .get(key)
                .await
                .map_err(db_error)?
                .map(|l| Node::new(WebhookLogNode(l))),
            None => None,
        },
        OUTBOUND_REQUEST_KIND => match unmarshal_int_id(kind, id) {
            Some(key) => db
                .outbound_requests()
                .get(key)
                .await
                .map_err(db_error)?
                .map(|r| Node::new(OutboundRequestNode(r))),
            None => None,
        },
        _ => {
            let Some(name) =
                unmarshal_id(id).and_then(|(_, spec)| serde_json::from_str::<String>(&spec).ok())
            else {
                return Ok(Some(None));
            };
            background_jobs(ctx, DEFAULT_RECENT_RUNS as usize)
                .await?
                .into_iter()
                .find(|j| j.name == name)
                .map(Node::new)
        }
    }))
}

/// Background job and admin log queries.
#[derive(Default)]
pub struct AdminLogsQuery;

#[Object]
impl AdminLogsQuery {
    /// The background jobs: the worker's leader-elected routines (with lease holders, recent
    /// runs and run statistics) and its job queues (with counts and recent failures). Site
    /// admins only.
    async fn background_jobs(
        &self,
        ctx: &Context<'_>,
        first: Option<i32>,
        after: Option<String>,
        recent_run_count: Option<i32>,
    ) -> Result<BackgroundJobConnection> {
        require_site_admin(ctx)?;
        let recent = usize::try_from(recent_run_count.unwrap_or(DEFAULT_RECENT_RUNS).max(0))
            .unwrap_or(0)
            .min(sourcegull_db::RECENT_RUNS_KEPT);
        let mut jobs = background_jobs(ctx, recent).await?;
        let total = jobs.len();
        let offset = usize::try_from(decode_cursor(after.as_deref())?).unwrap_or(0);
        let limit = page_size(first, MAX_PAGE, MAX_PAGE);
        let mut page: Vec<BackgroundJobNode> = jobs.drain(offset.min(total)..).collect();
        let page_info = PageInfo::new(i64::try_from(offset).unwrap_or(0), limit, page.len());
        page.truncate(usize::try_from(limit).unwrap_or(0));
        Ok(BackgroundJobConnection {
            nodes: page,
            total_count: to_int(i64::try_from(total).unwrap_or(0)),
            page_info,
        })
    }

    /// The newest outbound code host requests, newest first (`outboundRequestLogLimit` of
    /// them are kept; 0 turns the log off). Site admins only.
    async fn outbound_requests(
        &self,
        ctx: &Context<'_>,
        first: Option<i32>,
        after: Option<String>,
    ) -> Result<OutboundRequestConnection> {
        require_site_admin(ctx)?;
        let limit = page_size(first, MAX_PAGE, MAX_PAGE);
        let before = parse_id_cursor(after.as_deref())?;
        let store = services(ctx)?.db.outbound_requests();
        let mut rows = store.list(before, limit + 1).await.map_err(db_error)?;
        let total = store.count().await.map_err(db_error)?;
        let fetched = rows.len();
        rows.truncate(usize::try_from(limit).unwrap_or(0));
        Ok(OutboundRequestConnection {
            page_info: id_page_info(limit, fetched, rows.last().map(|r| r.id)),
            nodes: rows.into_iter().map(OutboundRequestNode).collect(),
            total_count: to_int(total),
        })
    }

    /// The newest slow GraphQL requests and searches, newest first (above
    /// `observability.logSlowGraphQLRequests` / `observability.logSlowSearches`). Site admins
    /// only.
    async fn slow_requests(
        &self,
        ctx: &Context<'_>,
        after: Option<String>,
        #[graphql(desc = "Page size (default 50).")] first: Option<i32>,
        #[graphql(desc = "Only this kind.")] kind: Option<SlowRequestKindEnum>,
    ) -> Result<SlowRequestConnection> {
        require_site_admin(ctx)?;
        let limit = page_size(first, 50, MAX_PAGE);
        let before = parse_id_cursor(after.as_deref())?;
        let kind = kind.map(|k| match k {
            SlowRequestKindEnum::Graphql => SlowRequestKind::GraphQl,
            SlowRequestKindEnum::Search => SlowRequestKind::Search,
        });
        let store = services(ctx)?.db.slow_requests();
        let mut rows = store
            .list(kind, before, limit + 1)
            .await
            .map_err(db_error)?;
        let total = store.count(kind).await.map_err(db_error)?;
        let fetched = rows.len();
        rows.truncate(usize::try_from(limit).unwrap_or(0));
        Ok(SlowRequestConnection {
            page_info: id_page_info(limit, fetched, rows.last().map(|r| r.id)),
            nodes: rows.into_iter().map(SlowRequestNode).collect(),
            total_count: to_int(total),
        })
    }

    /// Recently received code host webhooks across all connections, newest first. Site admins
    /// only. `webhookID` is taken as a code host connection ID (Sourcegull has one webhook per
    /// connection); `legacyOnly` is accepted and ignored.
    #[allow(clippy::too_many_arguments)]
    async fn webhook_logs(
        &self,
        ctx: &Context<'_>,
        first: Option<i32>,
        after: Option<String>,
        only_errors: Option<bool>,
        only_unmatched: Option<bool>,
        since: Option<DateTime<Utc>>,
        until: Option<DateTime<Utc>>,
        #[graphql(name = "webhookID")] webhook_id: Option<ID>,
        legacy_only: Option<bool>,
    ) -> Result<WebhookLogConnection> {
        let _ = legacy_only;
        let service = match webhook_id {
            Some(id) => Some(ExternalServiceId(
                unmarshal_int_id(sourcegull_services::relay::EXTERNAL_SERVICE_KIND, &id)
                    .ok_or_else(|| error(codes::INVALID_INPUT, "Invalid webhookID."))?,
            )),
            None => None,
        };
        webhook_logs(
            ctx,
            WebhookLogFilter {
                service,
                only_unmatched: only_unmatched.unwrap_or(false),
                only_errors: only_errors.unwrap_or(false),
                since,
                until,
                before_id: None,
            },
            first,
            after,
        )
        .await
    }
}

/// Background job mutations.
#[derive(Default)]
pub struct AdminLogsMutation;

#[Object]
impl AdminLogsMutation {
    /// Queues a failed, errored or canceled job of `queue` again with fresh retry counts.
    /// Site admins only.
    async fn retry_background_queue_job(
        &self,
        ctx: &Context<'_>,
        queue: String,
        id: ID,
    ) -> Result<EmptyResponse> {
        require_site_admin(ctx)?;
        let key = queue_job(ctx, &queue, &id).await?;
        let pool = services(ctx)?.db.pool();
        if !sourcegull_pgqueue::admin::retry(pool, &queue, key)
            .await
            .map_err(queue_error)?
        {
            return Err(error(
                codes::INVALID_INPUT,
                "Only failed, errored or canceled jobs can be retried.",
            ));
        }
        Ok(EmptyResponse)
    }

    /// Cancels a queued job of `queue` at once, or asks the worker of a processing one to stop
    /// it. Site admins only.
    async fn cancel_background_queue_job(
        &self,
        ctx: &Context<'_>,
        queue: String,
        id: ID,
    ) -> Result<EmptyResponse> {
        require_site_admin(ctx)?;
        let key = queue_job(ctx, &queue, &id).await?;
        let pool = services(ctx)?.db.pool();
        if !sourcegull_pgqueue::admin::cancel(pool, &queue, key)
            .await
            .map_err(queue_error)?
        {
            return Err(error(
                codes::INVALID_INPUT,
                "Only queued or processing jobs can be canceled.",
            ));
        }
        Ok(EmptyResponse)
    }
}

/// Variable names (lowercased, `_` removed) whose values are never stored.
const SECRET_VARIABLE_MARKERS: &[&str] = &[
    "token",
    "secret",
    "password",
    "passwd",
    "credential",
    "apikey",
    "privatekey",
    "authorization",
];

/// The variables of a GraphQL request as the slow request log stores them: none for a
/// mutation (its arguments carry secrets, credentials and connection configurations) or a
/// document that does not parse, and for a query the variables with values of secret-looking
/// names (at any depth) replaced by `REDACTED`.
pub(crate) fn slow_request_variables(
    query: &str,
    operation_name: Option<&str>,
    variables: &async_graphql::Variables,
) -> String {
    use async_graphql::parser::types::{DocumentOperations, OperationType};

    let Ok(doc) = async_graphql::parser::parse_query(query) else {
        return String::new();
    };
    let ty = match (&doc.operations, operation_name) {
        (DocumentOperations::Single(op), _) => Some(op.node.ty),
        (DocumentOperations::Multiple(ops), Some(name)) => ops
            .iter()
            .find(|(n, _)| n.as_str() == name)
            .map(|(_, op)| op.node.ty),
        // A named operation is parsed as one of several even when it is alone.
        (DocumentOperations::Multiple(ops), None) if ops.len() == 1 => {
            ops.values().next().map(|op| op.node.ty)
        }
        (DocumentOperations::Multiple(_), None) => None,
    };
    if ty != Some(OperationType::Query) {
        return String::new();
    }
    let mut value = serde_json::to_value(variables).unwrap_or_default();
    redact_secret_values(&mut value);
    serde_json::to_string(&value).unwrap_or_default()
}

fn redact_secret_values(v: &mut serde_json::Value) {
    match v {
        serde_json::Value::Object(map) => {
            for (k, v) in map.iter_mut() {
                let name = k.to_ascii_lowercase().replace('_', "");
                if SECRET_VARIABLE_MARKERS.iter().any(|m| name.contains(m)) {
                    *v = serde_json::Value::String("REDACTED".to_owned());
                } else {
                    redact_secret_values(v);
                }
            }
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(redact_secret_values),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::slow_request_variables;
    use async_graphql::Variables;
    use serde_json::json;

    #[test]
    fn slow_request_variables_leave_out_secrets() {
        let vars = Variables::from_json(json!({
            "first": 10,
            "accessToken": "glpat-1",
            "input": { "client_secret": "s", "names": ["a"] },
        }));
        let query = "query Q($first: Int) { currentUser { id } }";
        assert_eq!(
            slow_request_variables(query, None, &vars),
            r#"{"accessToken":"REDACTED","first":10,"input":{"client_secret":"REDACTED","names":["a"]}}"#
        );
        let mutation = "mutation M($value: String!) { createExecutorSecret(value: $value) { id } }";
        assert_eq!(slow_request_variables(mutation, None, &vars), "");
        let both = format!("{query} {mutation}");
        assert_eq!(slow_request_variables(&both, Some("M"), &vars), "");
        assert_ne!(slow_request_variables(&both, Some("Q"), &vars), "");
        assert_eq!(slow_request_variables(&both, None, &vars), "");
        assert_eq!(slow_request_variables("{ broken", None, &vars), "");
    }
}
