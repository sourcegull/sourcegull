//! Trends dashboards: `InsightsDashboard` with its views and grants, and the connection
//! `insightsDashboards` and `InsightView.dashboards` return.

use async_graphql::{Context, ID, Object, Result, SimpleObject};
use sourcegull_graphql_core::ids::{ORG_KIND, USER_KIND};
use sourcegull_insights::views::{Dashboard, Grant, Principal};
use sourcegull_services::relay::{marshal_id, unmarshal_id};

use super::view::{InsightViewNode, view_nodes};
use super::views_store;
use crate::context::{codes, db_error, error, page_size};
use crate::pagination::{PageInfo, decode_cursor};

/// Node kind of dashboard IDs: `base64("dashboard:{"IdType":"custom","Arg":<id>}")`
/// for (non-virtual) dashboards.
pub const DASHBOARD_KIND: &str = "dashboard";

pub fn dashboard_node_id(id: i64) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE.encode(format!(
        "{DASHBOARD_KIND}:{{\"IdType\":\"custom\",\"Arg\":{id}}}"
    ))
}

/// The id in a dashboard node ID ([`DASHBOARD_KIND`] form, or a bare number).
pub fn parse_dashboard_id(id: &str) -> Result<i64> {
    let invalid = || {
        error(
            codes::INVALID_INPUT,
            format!("Invalid dashboard ID {id:?}."),
        )
    };
    let (kind, spec) = unmarshal_id(id).ok_or_else(invalid)?;
    if kind != DASHBOARD_KIND {
        return Err(invalid());
    }
    if let Ok(n) = spec.parse::<i64>() {
        return Ok(n);
    }
    let v: serde_json::Value = serde_json::from_str(&spec).map_err(|_| invalid())?;
    if v.get("IdType").and_then(|t| t.as_str()) != Some("custom") {
        return Err(invalid());
    }
    v.get("Arg")
        .and_then(serde_json::Value::as_i64)
        .ok_or_else(invalid)
}

/// Grants of a dashboard.
#[derive(SimpleObject)]
pub struct InsightsPermissionGrants {
    /// Users that have permission.
    pub users: Vec<ID>,
    /// Organizations that have permission.
    pub organizations: Vec<ID>,
    /// Whether everyone has permission.
    pub global: bool,
}

impl InsightsPermissionGrants {
    pub fn from_grants(grants: &[Grant]) -> Self {
        let mut out = Self {
            users: Vec::new(),
            organizations: Vec::new(),
            global: false,
        };
        for g in grants {
            match g {
                Grant::User(u) => out.users.push(ID(marshal_id(USER_KIND, u.0))),
                Grant::Org(o) => out.organizations.push(ID(marshal_id(ORG_KIND, o.0))),
                Grant::Global => out.global = true,
            }
        }
        out
    }
}

/// A dashboard of trends.
pub struct InsightsDashboardNode(pub Dashboard);

#[Object(name = "InsightsDashboard")]
impl InsightsDashboardNode {
    pub async fn id(&self) -> ID {
        ID(dashboard_node_id(self.0.id))
    }

    async fn title(&self) -> &str {
        &self.0.title
    }

    /// The views on the dashboard, in the order they were added. Whoever sees the dashboard
    /// sees its views.
    async fn views(
        &self,
        ctx: &Context<'_>,
        first: Option<i32>,
        after: Option<ID>,
    ) -> Result<Option<InsightViewConnection>> {
        let offset = decode_cursor(after.as_ref().map(|a| a.as_str()))?;
        let limit = page_size(first, 50, 200);
        let total = self.0.views.len();
        let page: Vec<i64> = self
            .0
            .views
            .iter()
            .skip(usize::try_from(offset).unwrap_or(usize::MAX))
            .take(usize::try_from(limit).unwrap_or(usize::MAX) + 1)
            .copied()
            .collect();
        let page_info = PageInfo::new(offset, limit, page.len());
        let mut nodes = view_nodes(ctx, &page).await?;
        nodes.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        Ok(Some(InsightViewConnection {
            nodes,
            page_info,
            total_count: Some(i32::try_from(total).unwrap_or(i32::MAX)),
        }))
    }

    /// Who may see (and change) the dashboard.
    async fn grants(&self) -> InsightsPermissionGrants {
        InsightsPermissionGrants::from_grants(&self.0.grants)
    }
}

/// A page of trends.
pub struct InsightViewConnection {
    pub nodes: Vec<InsightViewNode>,
    pub page_info: PageInfo,
    pub total_count: Option<i32>,
}

#[Object]
impl InsightViewConnection {
    async fn nodes(&self) -> Vec<Option<&InsightViewNode>> {
        self.nodes.iter().map(Some).collect()
    }

    async fn page_info(&self) -> &PageInfo {
        &self.page_info
    }

    /// The number of views in the connection.
    async fn total_count(&self) -> Option<i32> {
        self.total_count
    }
}

/// A page of dashboards.
pub struct DashboardConnection {
    pub nodes: Vec<Dashboard>,
    pub page_info: PageInfo,
}

#[Object(name = "InsightsDashboardConnection")]
impl DashboardConnection {
    async fn nodes(&self) -> Vec<InsightsDashboardNode> {
        self.nodes
            .iter()
            .cloned()
            .map(InsightsDashboardNode)
            .collect()
    }

    async fn page_info(&self) -> &PageInfo {
        &self.page_info
    }
}

/// The dashboards `who` sees (all, the one `id`, or those holding view `with_view`), a page
/// after the dashboard `after` (a dashboard ID).
pub async fn list_dashboards(
    ctx: &Context<'_>,
    who: &Principal,
    first: Option<i32>,
    after: Option<String>,
    id: Option<ID>,
    with_view: Option<i64>,
) -> Result<DashboardConnection> {
    let after = match after.as_deref().filter(|a| !a.is_empty()) {
        Some(a) => Some(parse_dashboard_id(a)?),
        None => None,
    };
    let ids = match id {
        Some(id) => vec![parse_dashboard_id(&id)?],
        None => Vec::new(),
    };
    let limit = first.map(|f| i64::from(f.clamp(0, 1000)));
    let mut nodes = views_store(ctx)?
        .list_dashboards(who, &ids, with_view, after, limit.map(|l| l + 1))
        .await
        .map_err(db_error)?;
    let more = limit.is_some_and(|l| i64::try_from(nodes.len()).unwrap_or(i64::MAX) > l);
    if let Some(l) = limit {
        nodes.truncate(usize::try_from(l).unwrap_or(usize::MAX));
    }
    let page_info = PageInfo {
        has_next_page: more,
        end_cursor: if more {
            nodes.last().map(|d| dashboard_node_id(d.id))
        } else {
            None
        },
    };
    Ok(DashboardConnection { nodes, page_info })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dashboard_ids_round_trip() {
        let id = dashboard_node_id(42);
        assert_eq!(parse_dashboard_id(&id).unwrap(), 42);
        // base64("dashboard:{"IdType":"custom","Arg":42}")
        use base64::Engine as _;
        let raw = base64::engine::general_purpose::STANDARD
            .decode(&id)
            .unwrap();
        assert_eq!(
            String::from_utf8(raw).unwrap(),
            r#"dashboard:{"IdType":"custom","Arg":42}"#
        );
        assert!(parse_dashboard_id("VXNlcjox").is_err());
    }
}
