//! GitLab groups and their members, implementing [`GroupSource`], for
//! `own.syncTeamsFromGitLabGroups`.
//!
//! Groups come from `GET /groups?all_available=true` (with an admin token, every group of the
//! instance; otherwise the groups the token's user can see), subgroups included; members from
//! `GET /groups/:id/members` (direct members only, so a team's membership mirrors the
//! group's own). Blocked and deactivated users are skipped.

use std::sync::Arc;

use async_trait::async_trait;
use futures::TryStreamExt;
use serde::Deserialize;
use sourcegull_codehost::{CodeHostError, CodeHostGroup, GroupMember, GroupSource, Result};

use crate::client::{GitLabClient, PER_PAGE};
use crate::config::GitLabConnection;
use crate::source::SERVICE_TYPE;

/// The groups of one connection.
#[derive(Debug, Clone)]
pub struct GitLabGroups {
    service_id: String,
    client: Arc<GitLabClient>,
}

#[derive(Deserialize)]
struct Group {
    id: i64,
    full_path: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    parent_id: Option<i64>,
}

#[derive(Deserialize)]
struct Member {
    id: i64,
    username: String,
    #[serde(default)]
    state: Option<String>,
}

impl GitLabGroups {
    /// The group source of `conn`, reading with `client` (the connection's token and rate
    /// limiter).
    pub fn new(conn: &GitLabConnection, client: Arc<GitLabClient>) -> Result<Self> {
        let service_id = conn.base_url().map_err(CodeHostError::Config)?.to_string();
        Ok(Self { service_id, client })
    }
}

#[async_trait]
impl GroupSource for GitLabGroups {
    fn service_id(&self) -> &str {
        &self.service_id
    }

    fn service_type(&self) -> &str {
        SERVICE_TYPE
    }

    async fn list_groups(&self) -> Result<Vec<CodeHostGroup>> {
        let first = self.client.api_url(&format!(
            "groups?all_available=true&per_page={PER_PAGE}&order_by=id&sort=asc"
        ))?;
        let groups: Vec<Group> = self.client.paginate(first).try_collect().await?;
        Ok(groups
            .into_iter()
            .map(|g| CodeHostGroup {
                id: g.id.to_string(),
                name: g.name.unwrap_or_else(|| g.full_path.clone()),
                full_path: g.full_path,
                parent_id: g.parent_id.map(|p| p.to_string()),
            })
            .collect())
    }

    async fn group_members(&self, group_id: &str) -> Result<Vec<GroupMember>> {
        let id: String = url::form_urlencoded::byte_serialize(group_id.as_bytes()).collect();
        let first = self
            .client
            .api_url(&format!("groups/{id}/members?per_page={PER_PAGE}"))?;
        let members: Vec<Member> = self.client.paginate(first).try_collect().await?;
        Ok(members
            .into_iter()
            .filter(|m| m.state.as_deref().is_none_or(|s| s == "active"))
            .map(|m| GroupMember {
                account_id: m.id.to_string(),
                username: m.username,
            })
            .collect())
    }
}
