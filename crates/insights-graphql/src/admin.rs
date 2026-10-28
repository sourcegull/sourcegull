//! The admin backfill queue: `insightAdminBackfillQueue`, `retryInsightSeriesBackfill`,
//! `moveInsightSeriesBackfillToFrontOfQueue` and `moveInsightSeriesBackfillToBackOfQueue`,
//! for site admins only. The engine side is
//! `sourcegull_insights::admin`.

use async_graphql::{Context, Enum, ID, Object, Result, SimpleObject};
use chrono::{DateTime, TimeDelta, Utc};
use sourcegull_graphql_core::shared::UserRef;
use sourcegull_insights::admin::{
    QueueChange, QueueFilter, QueueItem, QueueOrder, QueueState, sort_queue,
};
use sourcegull_insights::{InsightStore, access};
use sourcegull_services::relay::unmarshal_id;

use crate::context::{access_error, codes, db_error, error, services, to_int, viewer};
use crate::pagination::PageArgs;

pub use crate::pagination::ConnectionPageInfo;

/// Kind of a queue item's ID.
pub const BACKFILL_ITEM_KIND: &str = "backfillQueueItem";

const DEFAULT_PAGE: i64 = 50;
const MAX_PAGE: i64 = 1000;

/// The ID of a queue item: its series and view (`BackfillQueueID`).
pub fn backfill_item_id(series_id: i64, view_unique_id: &str) -> String {
    use base64::Engine as _;
    let spec = serde_json::json!({ "BackfillID": series_id, "InsightID": view_unique_id });
    base64::engine::general_purpose::URL_SAFE.encode(format!("{BACKFILL_ITEM_KIND}:{spec}"))
}

/// The series and view of a queue item ID.
pub fn parse_backfill_item_id(id: &str) -> Result<(i64, String)> {
    let invalid = || {
        error(
            codes::INVALID_INPUT,
            format!("Invalid backfill queue item ID {id:?}."),
        )
    };
    let (kind, spec) = unmarshal_id(id).ok_or_else(invalid)?;
    if kind != BACKFILL_ITEM_KIND {
        return Err(invalid());
    }
    let v: serde_json::Value = serde_json::from_str(&spec).map_err(|_| invalid())?;
    let series = v
        .get("BackfillID")
        .and_then(|b| b.as_i64())
        .ok_or_else(invalid)?;
    let view = v
        .get("InsightID")
        .and_then(|b| b.as_str())
        .unwrap_or_default();
    Ok((series, view.to_owned()))
}

/// How the backfill queue is sorted.
#[derive(Enum, Copy, Clone, Debug, PartialEq, Eq, Default)]
#[graphql(rename_items = "SCREAMING_SNAKE_CASE")]
pub enum BackfillQueueOrderBy {
    #[default]
    State,
    QueuePosition,
}

/// Where a backfill is.
#[derive(Enum, Copy, Clone, Debug, PartialEq, Eq)]
#[graphql(rename_items = "SCREAMING_SNAKE_CASE")]
pub enum InsightQueueItemState {
    New,
    Queued,
    Completed,
    Processing,
    Failed,
    Unknown,
}

impl From<QueueState> for InsightQueueItemState {
    fn from(s: QueueState) -> Self {
        match s {
            QueueState::New => Self::New,
            QueueState::Queued => Self::Queued,
            QueueState::Processing => Self::Processing,
            QueueState::Completed => Self::Completed,
            QueueState::Failed => Self::Failed,
        }
    }
}

impl InsightQueueItemState {
    fn to_engine(self) -> Option<QueueState> {
        Some(match self {
            Self::New => QueueState::New,
            Self::Queued => QueueState::Queued,
            Self::Processing => QueueState::Processing,
            Self::Completed => QueueState::Completed,
            Self::Failed => QueueState::Failed,
            Self::Unknown => return None,
        })
    }
}

pub struct InsightBackfillQueueItem(pub QueueItem);

/// Information about an item in the trends backfill queue.
#[Object]
impl InsightBackfillQueueItem {
    /// The ID of the backfill item.
    async fn id(&self) -> ID {
        ID(backfill_item_id(self.0.series_id, &self.0.view_unique_id))
    }

    /// The title of the trend.
    async fn insight_view_title(&self) -> &str {
        &self.0.view_title
    }

    /// The user that originally created the trend.
    async fn creator(&self, ctx: &Context<'_>) -> Result<Option<UserRef>> {
        let Some(id) = self.0.creator else {
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

    /// The name of the series on the associated trend.
    async fn series_label(&self) -> &str {
        &self.0.label
    }

    /// The search query for the series.
    async fn series_search_query(&self) -> &str {
        &self.0.query
    }

    /// Information on the current backfill status.
    async fn backfill_queue_status(&self) -> BackfillStatus<'_> {
        BackfillStatus(&self.0)
    }
}

pub struct BackfillStatus<'a>(&'a QueueItem);

/// The status of a trends backfill.
#[Object]
impl BackfillStatus<'_> {
    /// The current backfill queue state.
    async fn state(&self) -> InsightQueueItemState {
        self.0.state.into()
    }

    /// If the item is queued, its position in the queue.
    async fn queue_position(&self) -> Option<i32> {
        self.0.queue_position.map(to_int)
    }

    /// A list of errors that have occurred during backfilling.
    async fn errors(&self) -> Option<&[String]> {
        Some(&self.0.errors)
    }

    /// The estimated cost of the backfill process (repositories times points).
    async fn cost(&self) -> Option<i32> {
        self.0.cost.map(to_int)
    }

    /// The percent complete of the backfill.
    async fn percent_complete(&self) -> Option<i32> {
        self.0.percent_complete
    }

    /// The date/time when the backfill was created.
    async fn created_at(&self) -> Option<DateTime<Utc>> {
        Some(self.0.created_at)
    }

    /// The date/time when the backfill started processing.
    async fn started_at(&self) -> Option<DateTime<Utc>> {
        self.0.started_at
    }

    /// The date/time when the backfill completed processing.
    async fn completed_at(&self) -> Option<DateTime<Utc>> {
        self.0.completed_at
    }

    /// The actual runtime duration spent processing the backfill.
    async fn runtime(&self) -> Option<String> {
        self.0.runtime().map(go_duration)
    }
}

/// Returns information regarding the trends backfill queue to admin users.
#[derive(SimpleObject)]
pub struct InsightBackfillQueueItemConnection {
    /// A list of trend backfill queue items.
    pub nodes: Vec<InsightBackfillQueueItem>,
    /// The total number of trend backfill queue items in the connection.
    pub total_count: i32,
    /// Pagination information.
    pub page_info: ConnectionPageInfo,
}

/// A duration as Go's `time.Duration.String` writes it (`1h2m3.5s`, `150ms`), as
/// `runtime` reports it.
pub fn go_duration(d: TimeDelta) -> String {
    let Some(ns) = d.num_nanoseconds() else {
        return format!("{}h0m0s", d.num_hours());
    };
    if ns == 0 {
        return "0s".into();
    }
    let sign = if ns < 0 { "-" } else { "" };
    let ns = ns.unsigned_abs();
    let frac = |v: u64, unit: u64, digits: usize| {
        let (int, rem) = (v / unit, v % unit);
        if rem == 0 {
            int.to_string()
        } else {
            let f = format!("{rem:0digits$}");
            format!("{int}.{}", f.trim_end_matches('0'))
        }
    };
    let body = if ns < 1_000 {
        format!("{ns}ns")
    } else if ns < 1_000_000 {
        format!("{}µs", frac(ns, 1_000, 3))
    } else if ns < 1_000_000_000 {
        format!("{}ms", frac(ns, 1_000_000, 6))
    } else {
        let (h, m) = (ns / 3_600_000_000_000, ns / 60_000_000_000 % 60);
        let s = format!("{}s", frac(ns % 60_000_000_000, 1_000_000_000, 9));
        if h > 0 {
            format!("{h}h{m}m{s}")
        } else if m > 0 {
            format!("{m}m{s}")
        } else {
            s
        }
    };
    format!("{sign}{body}")
}

fn store(ctx: &Context<'_>) -> Result<InsightStore> {
    Ok(InsightStore::new(&services(ctx)?.db))
}

/// The backfill queue page (see `insightAdminBackfillQueue`).
#[allow(clippy::too_many_arguments)]
pub async fn backfill_queue(
    ctx: &Context<'_>,
    first: Option<i32>,
    last: Option<i32>,
    after: Option<String>,
    before: Option<String>,
    order_by: BackfillQueueOrderBy,
    descending: bool,
    states: Option<Vec<InsightQueueItemState>>,
    text_search: Option<String>,
) -> Result<InsightBackfillQueueItemConnection> {
    // 🚨 SECURITY: the queue shows every trend's title and query: site admins only.
    access::require_queue_admin(viewer(ctx)).map_err(access_error)?;
    let page = PageArgs {
        first,
        last,
        after,
        before,
    };
    page.check()?;
    let states = states.unwrap_or_default();
    // `UNKNOWN` never matches; asking only for it lists nothing.
    let only_unknown =
        !states.is_empty() && states.iter().all(|s| *s == InsightQueueItemState::Unknown);
    let filter = QueueFilter {
        states: states.iter().filter_map(|s| s.to_engine()).collect(),
        text: text_search,
        series_id: None,
    };
    let mut items = if only_unknown {
        Vec::new()
    } else {
        store(ctx)?
            .backfill_queue(&filter)
            .await
            .map_err(db_error)?
    };
    sort_queue(
        &mut items,
        match order_by {
            BackfillQueueOrderBy::State => QueueOrder::State,
            BackfillQueueOrderBy::QueuePosition => QueueOrder::QueuePosition,
        },
        descending,
    );
    let total = items.len() as i64;
    let (start, end) = page.window(total, DEFAULT_PAGE, MAX_PAGE)?;
    let nodes: Vec<InsightBackfillQueueItem> = items
        .drain(start as usize..end as usize)
        .map(InsightBackfillQueueItem)
        .collect();
    Ok(InsightBackfillQueueItemConnection {
        page_info: ConnectionPageInfo::new(start, end, total),
        nodes,
        total_count: to_int(total),
    })
}

/// What a queue mutation does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueAction {
    Retry,
    Front,
    Back,
}

fn state_name(s: QueueState) -> &'static str {
    match s {
        QueueState::New => "NEW",
        QueueState::Queued => "QUEUED",
        QueueState::Processing => "PROCESSING",
        QueueState::Completed => "COMPLETED",
        QueueState::Failed => "FAILED",
    }
}

/// Retries or moves the backfill of queue item `id`, then returns the item.
pub async fn change_backfill(
    ctx: &Context<'_>,
    id: &ID,
    action: QueueAction,
) -> Result<InsightBackfillQueueItem> {
    // 🚨 SECURITY: site admins only.
    access::require_queue_admin(viewer(ctx)).map_err(access_error)?;
    let (series, view) = parse_backfill_item_id(id)?;
    let store = store(ctx)?;
    let changed = match action {
        QueueAction::Retry => store.retry_backfill(series).await,
        QueueAction::Front => store.move_backfill(series, true).await,
        QueueAction::Back => store.move_backfill(series, false).await,
    }
    .map_err(db_error)?;
    let not_found = || error(codes::NOT_FOUND, "Backfill not found.");
    match changed {
        QueueChange::Done => {}
        QueueChange::NotFound => return Err(not_found()),
        QueueChange::WrongState(s) => {
            let message = if action == QueueAction::Retry {
                format!(
                    "Only backfills that have finished can be retried (current state {}).",
                    state_name(s)
                )
            } else {
                format!(
                    "Only backfills that have not finished can have their priority changed \
                     (current state {}).",
                    state_name(s)
                )
            };
            return Err(error(codes::CONFLICT, message));
        }
    }
    let mut items = store
        .backfill_queue(&QueueFilter {
            series_id: Some(series),
            ..QueueFilter::default()
        })
        .await
        .map_err(db_error)?;
    let at = items
        .iter()
        .position(|i| i.view_unique_id == view)
        .unwrap_or(0);
    if items.is_empty() {
        return Err(not_found());
    }
    Ok(InsightBackfillQueueItem(items.swap_remove(at)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn go_durations() {
        let ms = TimeDelta::milliseconds;
        assert_eq!(go_duration(TimeDelta::zero()), "0s");
        assert_eq!(go_duration(ms(150)), "150ms");
        assert_eq!(go_duration(TimeDelta::microseconds(1500)), "1.5ms");
        assert_eq!(go_duration(TimeDelta::microseconds(12)), "12µs");
        assert_eq!(go_duration(ms(2500)), "2.5s");
        assert_eq!(go_duration(ms(90_000)), "1m30s");
        assert_eq!(go_duration(TimeDelta::hours(1)), "1h0m0s");
        assert_eq!(go_duration(ms(3_723_004)), "1h2m3.004s");
        assert_eq!(go_duration(-ms(2000)), "-2s");
    }

    #[test]
    fn item_ids_round_trip() {
        let id = backfill_item_id(7, "abc");
        assert_eq!(parse_backfill_item_id(&id).unwrap(), (7, "abc".to_owned()));
        assert!(parse_backfill_item_id("bogus").is_err());
        let other = sourcegull_services::relay::marshal_id("User", 7);
        assert!(parse_backfill_item_id(&other).is_err());
    }
}
