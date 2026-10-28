//! Groups and their members on the code host: [`GroupSource`], for syncing teams from code
//! host groups (`own.syncTeamsFromGitLabGroups`).

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::Result;

/// A group on the code host (a GitLab group or subgroup).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeHostGroup {
    /// The group's id on the code host.
    pub id: String,
    /// The full path, e.g. `acme/backend`.
    pub full_path: String,
    /// The human-readable name.
    pub name: String,
    /// The parent group's id; `None` for a top-level group.
    pub parent_id: Option<String>,
}

/// A direct member of a group.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupMember {
    /// The account id on the code host (matched against external accounts).
    pub account_id: String,
    /// The username on the code host.
    pub username: String,
}

/// Lists groups and their members with a connection's credentials.
#[async_trait]
pub trait GroupSource: Send + Sync {
    /// The code host instance (`ExternalRepoSpec::service_id`; external accounts carry it).
    fn service_id(&self) -> &str;

    /// The code host type (`gitlab`).
    fn service_type(&self) -> &str;

    /// Every group the connection's credentials can see.
    async fn list_groups(&self) -> Result<Vec<CodeHostGroup>>;

    /// The active direct members of a group (not inherited from parent groups).
    async fn group_members(&self, group_id: &str) -> Result<Vec<GroupMember>>;
}
