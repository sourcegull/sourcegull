//! Search Jobs: the `SearchJob` type, `searchJobs`, `validateSearchJob` and the
//! create, cancel and delete mutations.
//!
//! Only signed-in users use search jobs. A job is visible to its creator and to site admins,
//! who may also filter by creator (`userIDs`); others get `FORBIDDEN` for its mutations and
//! `null` from `node(id:)`.

use async_graphql::{Context, Enum, ID, Object, Result, SimpleObject};
use chrono::{DateTime, Utc};
use sourcegull_authn::Actor;
use sourcegull_db::DbError;
use sourcegull_searchjobs::{
    JobOrder, ListSearchJobs, SearchJob, SearchJobStore, access, can_access,
};
use sourcegull_services::relay::{marshal_id, unmarshal_int_id};
use sourcegull_types::UserId;

mod feature;

pub use feature::SearchJobsGraphql;

// `crate::context` and `crate::pagination`, as in the schema assembly.
use sourcegull_graphql_core::{context, pagination};

use crate::context::{
    access_error, codes, db_error, error, require_viewer, services, to_int, validation, viewer,
};
use crate::pagination::{decode_cursor, encode_cursor};
use sourcegull_graphql_core::EmptyResponse;
use sourcegull_graphql_core::ids::USER_KIND;
use sourcegull_graphql_core::node::{NodeFuture, NodeLoader};
use sourcegull_graphql_core::shared::UserRef;

/// Node kind of search job IDs.
pub const SEARCH_JOB_KIND: &str = "SearchJob";

/// Jobs per page without `first:`/`last:`.
const DEFAULT_PAGE: i64 = 50;
/// Most jobs per page.
const MAX_PAGE: i64 = 1000;

/// `SearchJob.state`.
#[derive(Enum, Copy, Clone, Eq, PartialEq, Debug)]
pub enum SearchJobState {
    /// Created, waiting to be processed.
    Queued,
    /// Being processed.
    Processing,
    /// Had an error (retried).
    Errored,
    /// Failed.
    Failed,
    /// Completed.
    Completed,
    /// Canceled.
    Canceled,
}

impl SearchJobState {
    fn from_db(s: &str) -> Self {
        match s {
            "processing" => Self::Processing,
            "errored" => Self::Errored,
            "failed" => Self::Failed,
            "completed" => Self::Completed,
            "canceled" => Self::Canceled,
            _ => Self::Queued,
        }
    }

    fn as_db(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Processing => "processing",
            Self::Errored => "errored",
            Self::Failed => "failed",
            Self::Completed => "completed",
            Self::Canceled => "canceled",
        }
    }
}

/// Sort order of `searchJobs`.
#[derive(Enum, Copy, Clone, Eq, PartialEq, Debug, Default)]
pub enum SearchJobsOrderBy {
    /// By query.
    Query,
    /// By creation date.
    #[default]
    CreatedAt,
    /// By state.
    State,
}

/// A search job.
#[derive(Debug, Clone)]
pub struct SearchJobNode(pub SearchJob);

#[Object(name = "SearchJob")]
impl SearchJobNode {
    /// The opaque node ID.
    pub async fn id(&self) -> ID {
        ID(marshal_id(SEARCH_JOB_KIND, self.0.id))
    }

    /// The query it runs.
    async fn query(&self) -> &str {
        &self.0.query
    }

    /// The state of the whole job, over all its repositories and revisions.
    async fn state(&self) -> SearchJobState {
        SearchJobState::from_db(&self.0.state)
    }

    /// The user who created it; null once deleted.
    async fn creator(&self, ctx: &Context<'_>) -> Result<Option<UserRef>> {
        Ok(services(ctx)?
            .db
            .users()
            .get_by_id(self.0.initiator_id)
            .await
            .map_err(db_error)?
            .map(UserRef))
    }

    async fn created_at(&self) -> DateTime<Utc> {
        self.0.created_at
    }

    async fn started_at(&self) -> Option<DateTime<Utc>> {
        self.0.started_at
    }

    /// When its last part finished, once the whole job has.
    async fn finished_at(&self) -> Option<DateTime<Utc>> {
        self.0.finished_at
    }

    /// Where to download the results (JSON lines; `.csv` instead of `.jsonl` gives CSV).
    #[graphql(name = "URL")]
    async fn url(&self, ctx: &Context<'_>) -> Result<Option<String>> {
        Ok(Some(export_url(ctx, self.0.id, "jsonl")?))
    }

    /// Where to download the log: one CSV row per searched repository revision.
    #[graphql(name = "logURL")]
    async fn log_url(&self, ctx: &Context<'_>) -> Result<Option<String>> {
        Ok(Some(export_url(ctx, self.0.id, "log")?))
    }

    /// Progress over the repository revisions it searches.
    async fn repo_stats(&self, ctx: &Context<'_>) -> Result<SearchJobStats> {
        let s = store(ctx)?
            .repo_rev_stats(self.0.id)
            .await
            .map_err(db_error)?;
        Ok(SearchJobStats {
            total: to_int(s.total),
            completed: to_int(s.completed),
            failed: to_int(s.failed),
            in_progress: to_int(s.in_progress),
        })
    }
}

/// `/.api/search/export/{id}.{ext}`, absolute with `externalURL`.
fn export_url(ctx: &Context<'_>, id: i64, ext: &str) -> Result<String> {
    let base = services(ctx)?.config.load().value.external_url.clone();
    Ok(format!(
        "{}/.api/search/export/{id}.{ext}",
        base.unwrap_or_default().trim_end_matches('/')
    ))
}

/// Progress of a search job's repository revisions.
#[derive(SimpleObject, Debug, Clone, Copy)]
pub struct SearchJobStats {
    /// All revisions.
    pub total: i32,
    /// Revisions searched.
    pub completed: i32,
    /// Revisions that failed.
    pub failed: i32,
    /// Revisions waiting or being searched.
    pub in_progress: i32,
}

/// Page info for paging both ways.
#[derive(SimpleObject, Debug, Clone, PartialEq, Eq)]
pub struct BidirectionalPageInfo {
    pub has_previous_page: bool,
    pub has_next_page: bool,
    /// Pass as `before` for the previous page.
    pub start_cursor: Option<String>,
    /// Pass as `after` for the next page.
    pub end_cursor: Option<String>,
}

/// A page of search jobs.
#[derive(SimpleObject)]
pub struct SearchJobConnection {
    pub nodes: Vec<SearchJobNode>,
    /// All jobs matching the filters.
    pub total_count: i32,
    pub page_info: BidirectionalPageInfo,
}

fn store(ctx: &Context<'_>) -> Result<SearchJobStore> {
    Ok(SearchJobStore::new(services(ctx)?.db.pool().clone()))
}

fn parse_job_id(id: &ID) -> Result<i64> {
    unmarshal_int_id(SEARCH_JOB_KIND, id).ok_or_else(|| {
        error(
            codes::INVALID_INPUT,
            format!("Invalid search job ID {:?}.", id.0),
        )
    })
}

/// The job with `id` if `actor` may manage it: `NOT_FOUND` or `FORBIDDEN` otherwise.
async fn load_for(ctx: &Context<'_>, actor: &Actor, id: &ID) -> Result<SearchJob> {
    access::job_for(&store(ctx)?, actor, parse_job_id(id)?)
        .await
        .map_err(access_error)
}

/// Loads a job for `node(id:)`: `None` unless the viewer may see it.
pub async fn load_node(ctx: &Context<'_>, id: i64) -> Result<Option<SearchJobNode>> {
    let Some(actor) = viewer(ctx) else {
        return Ok(None);
    };
    let job = store(ctx)?.get(id).await.map_err(db_error)?;
    Ok(job
        .filter(|j| can_access(actor, j.initiator_id))
        .map(SearchJobNode))
}

/// The `node(id:)` loader of search jobs.
pub fn node_loaders<N: From<SearchJobNode> + Send + 'static>() -> [NodeLoader<N>; 1] {
    fn load<'a, N: From<SearchJobNode> + Send + 'static>(
        ctx: &'a Context<'_>,
        _kind: &'a str,
        id: &'a str,
    ) -> NodeFuture<'a, N> {
        Box::pin(async move {
            let Some(key) = unmarshal_int_id(SEARCH_JOB_KIND, id) else {
                return Ok(None);
            };
            Ok(load_node(ctx, key).await?.map(N::from))
        })
    }
    [NodeLoader {
        kinds: &[SEARCH_JOB_KIND],
        load: load::<N>,
    }]
}

/// The search jobs queries.
#[derive(Default)]
pub struct SearchJobsQuery;

#[Object]
impl SearchJobsQuery {
    /// Checks that `query` can run as a search job; fails with the reason otherwise.
    async fn validate_search_job(
        &self,
        ctx: &Context<'_>,
        query: String,
    ) -> Result<Option<EmptyResponse>> {
        require_viewer(ctx)?;
        sourcegull_searchjobs::validate(&query).map_err(|e| validation(vec![e]))?;
        Ok(None)
    }

    /// Search jobs: the viewer's own, or (site admins) everyone's or those of `userIDs`.
    #[allow(clippy::too_many_arguments)]
    async fn search_jobs(
        &self,
        ctx: &Context<'_>,
        first: Option<i32>,
        last: Option<i32>,
        after: Option<String>,
        before: Option<String>,
        #[graphql(name = "userIDs")] user_ids: Option<Vec<ID>>,
        query: Option<String>,
        states: Option<Vec<SearchJobState>>,
        #[graphql(default)] order_by: SearchJobsOrderBy,
        #[graphql(default)] descending: bool,
    ) -> Result<SearchJobConnection> {
        let actor = require_viewer(ctx)?;
        if first.is_some() && last.is_some() {
            return Err(error(
                codes::INVALID_INPUT,
                "Use either first or last, not both.",
            ));
        }
        let ids = user_ids
            .unwrap_or_default()
            .iter()
            .map(|id| {
                unmarshal_int_id(USER_KIND, id).map(UserId).ok_or_else(|| {
                    error(codes::INVALID_INPUT, format!("Invalid user ID {:?}.", id.0))
                })
            })
            .collect::<Result<Vec<_>>>()?;
        // 🚨 SECURITY: users see their own jobs; only site admins see or filter by others'.
        let initiators = access::list_initiators(actor, ids).map_err(access_error)?;
        let mut opts = ListSearchJobs {
            query: query.filter(|q| !q.is_empty()),
            states: states
                .unwrap_or_default()
                .into_iter()
                .map(|s| s.as_db().to_owned())
                .collect(),
            initiators,
            order: match order_by {
                SearchJobsOrderBy::Query => JobOrder::Query,
                SearchJobsOrderBy::CreatedAt => JobOrder::CreatedAt,
                SearchJobsOrderBy::State => JobOrder::State,
            },
            descending,
            limit: 0,
            offset: 0,
        };
        let store = store(ctx)?;
        let total = store.count(&opts).await.map_err(db_error)?;
        // Cursors are boundaries between positions: `after: c` starts at c, `before: c` ends
        // there.
        let size = |n: Option<i32>| n.map_or(DEFAULT_PAGE, |n| i64::from(n).clamp(0, MAX_PAGE));
        let (start, end) = if let Some(last) = last {
            let end = match before.as_deref() {
                Some(c) => decode_cursor(Some(c))?.min(total),
                None => total,
            };
            ((end - size(Some(last))).max(0), end)
        } else {
            let start = decode_cursor(after.as_deref())?;
            let mut end = (start + size(first)).min(total);
            if let Some(c) = before.as_deref() {
                end = end.min(decode_cursor(Some(c))?);
            }
            (start, end.max(start))
        };
        opts.offset = start;
        opts.limit = end - start;
        let nodes = if opts.limit > 0 {
            store.list(&opts).await.map_err(db_error)?
        } else {
            Vec::new()
        };
        let end = start + nodes.len() as i64;
        Ok(SearchJobConnection {
            page_info: BidirectionalPageInfo {
                has_previous_page: start > 0,
                has_next_page: end < total,
                start_cursor: (!nodes.is_empty()).then(|| encode_cursor(start)),
                end_cursor: (!nodes.is_empty()).then(|| encode_cursor(end)),
            },
            nodes: nodes.into_iter().map(SearchJobNode).collect(),
            total_count: to_int(total),
        })
    }
}

/// The search jobs mutations.
#[derive(Default)]
pub struct SearchJobsMutation;

#[Object]
impl SearchJobsMutation {
    /// Creates a search job for `query` after validating it (as `validateSearchJob`).
    async fn create_search_job(&self, ctx: &Context<'_>, query: String) -> Result<SearchJobNode> {
        let actor = require_viewer(ctx)?;
        sourcegull_searchjobs::validate(&query).map_err(|e| validation(vec![e]))?;
        match store(ctx)?.create(actor.user_id, &query).await {
            Ok(job) => Ok(SearchJobNode(job)),
            Err(DbError::AlreadyExists { .. }) => Err(error(
                codes::ALREADY_EXISTS,
                "You already have a search job for this query.",
            )),
            Err(e) => Err(db_error(e)),
        }
    }

    /// Cancels what has not finished of a search job.
    async fn cancel_search_job(&self, ctx: &Context<'_>, id: ID) -> Result<Option<EmptyResponse>> {
        let actor = require_viewer(ctx)?;
        let job = load_for(ctx, actor, &id).await?;
        store(ctx)?.cancel(job.id).await.map_err(db_error)?;
        Ok(None)
    }

    /// Deletes a search job with its results.
    async fn delete_search_job(&self, ctx: &Context<'_>, id: ID) -> Result<EmptyResponse> {
        let actor = require_viewer(ctx)?;
        let job = load_for(ctx, actor, &id).await?;
        let blobs = services(ctx)?.app.as_ref().and_then(|a| a.blobs.clone());
        sourcegull_searchjobs::delete_job(&store(ctx)?, blobs.as_ref(), job.id)
            .await
            .map_err(|e| match e {
                sourcegull_searchjobs::Error::Db(e) => db_error(e),
                other => {
                    tracing::error!(error = %other, job = job.id, "deleting search job results");
                    error(codes::INTERNAL, "Internal error.")
                }
            })?;
        Ok(EmptyResponse)
    }
}
