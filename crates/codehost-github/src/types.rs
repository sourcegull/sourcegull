//! GitHub REST API objects (the fields Sourcegull uses).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A repository (`GET /repos/{owner}/{repo}`, and the items of repository lists).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Repository {
    pub id: i64,
    /// The GraphQL node id.
    #[serde(default)]
    pub node_id: String,
    /// `owner/name`.
    pub full_name: String,
    #[serde(default)]
    pub description: Option<String>,
    pub html_url: String,
    #[serde(default)]
    pub clone_url: String,
    #[serde(default)]
    pub ssh_url: String,
    #[serde(default)]
    pub default_branch: Option<String>,
    #[serde(default)]
    pub private: bool,
    /// `public`, `private` or `internal` (GitHub Enterprise); absent on old instances.
    #[serde(default)]
    pub visibility: Option<String>,
    #[serde(default)]
    pub archived: bool,
    /// GitHub disabled the repository (e.g. a terms of service violation).
    #[serde(default)]
    pub disabled: bool,
    #[serde(default)]
    pub fork: bool,
    #[serde(default)]
    pub stargazers_count: i64,
    #[serde(default)]
    pub forks_count: i64,
    #[serde(default)]
    pub topics: Vec<String>,
    /// Size in KiB; 0 for a repository without commits.
    #[serde(default)]
    pub size: i64,
    #[serde(default)]
    pub pushed_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub updated_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub created_at: Option<DateTime<Utc>>,
    /// The token user's access, when the API reports it.
    #[serde(default)]
    pub permissions: Option<RepoPermissions>,
}

impl Repository {
    /// The visibility, from `visibility` or, on instances without it, `private`.
    pub fn visibility(&self) -> &str {
        match self.visibility.as_deref() {
            Some(v) => v,
            None if self.private => "private",
            None => "public",
        }
    }

    /// The newer of the last push and the last update.
    pub fn last_activity_at(&self) -> Option<DateTime<Utc>> {
        self.pushed_at.max(self.updated_at)
    }
}

/// `permissions` of a repository.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoPermissions {
    #[serde(default)]
    pub admin: bool,
    #[serde(default)]
    pub push: bool,
    #[serde(default)]
    pub pull: bool,
}

/// `GET /search/repositories`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct RepositorySearch {
    #[serde(default)]
    pub total_count: i64,
    #[serde(default)]
    pub incomplete_results: bool,
    #[serde(default)]
    pub items: Vec<Repository>,
}

/// `GET /user`, the fields Sourcegull reads.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct User {
    pub id: i64,
    pub login: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repository_tolerates_missing_fields() {
        let r: Repository = serde_json::from_str(
            r#"{"id": 1, "full_name": "a/b", "html_url": "https://github.com/a/b",
                "private": true, "description": null}"#,
        )
        .unwrap();
        assert_eq!(r.visibility(), "private");
        assert!(!r.fork);
        assert_eq!(r.last_activity_at(), None);
    }
}
