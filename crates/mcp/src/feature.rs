//! The MCP server as a server feature: its routes.

use axum::Router;
use sourcegull_feature::Feature;
use sourcegull_services::AppState;

/// The MCP endpoint ([`crate::routes`]).
pub struct McpFeature;

impl Feature<AppState> for McpFeature {
    fn name(&self) -> &'static str {
        "mcp"
    }

    fn routes(&self) -> Option<Router<AppState>> {
        Some(crate::routes())
    }
}
