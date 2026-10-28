//! GitLab API objects (the fields Sourcegull uses).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A project (`GET /projects/:id`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Project {
    pub id: i64,
    pub path_with_namespace: String,
    #[serde(default)]
    pub description: Option<String>,
    pub web_url: String,
    #[serde(default)]
    pub http_url_to_repo: String,
    #[serde(default)]
    pub ssh_url_to_repo: String,
    #[serde(default)]
    pub default_branch: Option<String>,
    /// `public`, `internal` or `private`.
    #[serde(default = "default_visibility")]
    pub visibility: String,
    #[serde(default)]
    pub archived: bool,
    #[serde(default)]
    pub star_count: i64,
    #[serde(default)]
    pub forks_count: i64,
    #[serde(default)]
    pub topics: Vec<String>,
    /// Older GitLab versions report topics here.
    #[serde(default)]
    pub tag_list: Vec<String>,
    #[serde(default)]
    pub last_activity_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub created_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub empty_repo: bool,
    /// Present (non-null) for forks.
    #[serde(default)]
    pub forked_from_project: Option<ForkedFrom>,
    /// Who may read the repository: `enabled`, `private` (project members only) or
    /// `disabled` (the repository feature is switched off). Absent from `simple` listings.
    #[serde(default)]
    pub repository_access_level: Option<String>,
    /// The token user's access to the project, when the API reports it (not for `simple`
    /// listings, and `null` levels for non-members).
    #[serde(default)]
    pub permissions: Option<ProjectPermissions>,
}

/// GitLab's Reporter access level, the lowest that may download a private project's code.
pub const REPORTER_ACCESS: i64 = 20;

impl Project {
    /// Whether the project's repository feature is switched off.
    pub fn repository_disabled(&self) -> bool {
        self.repository_access_level.as_deref() == Some("disabled")
    }

    /// The token user's effective access level: the higher of the project and group
    /// membership levels, `None` when the API reports neither.
    pub fn access_level(&self) -> Option<i64> {
        let p = self.permissions.as_ref()?;
        let level = |a: &Option<AccessLevel>| a.as_ref().map(|a| a.access_level);
        level(&p.project_access).max(level(&p.group_access))
    }
}

/// `permissions` of a project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectPermissions {
    #[serde(default)]
    pub project_access: Option<AccessLevel>,
    #[serde(default)]
    pub group_access: Option<AccessLevel>,
}

/// One membership level in [`ProjectPermissions`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccessLevel {
    pub access_level: i64,
}

fn default_visibility() -> String {
    "private".to_owned()
}

/// The parent of a fork.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForkedFrom {
    pub id: i64,
    #[serde(default)]
    pub path_with_namespace: Option<String>,
}

/// `GET /version`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionInfo {
    pub version: String,
    #[serde(default)]
    pub revision: String,
}

/// `GET /personal_access_tokens/self`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenInfo {
    #[serde(default)]
    pub scopes: Vec<String>,
    #[serde(default)]
    pub active: Option<bool>,
}

/// A parsed GitLab version, e.g. `16.5.1-ee`.
/// The job of a CI job token (`GET /job` with `JOB-TOKEN`), the fields Sourcegull reads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CiJob {
    pub id: i64,
    /// The branch or tag the job runs for.
    #[serde(rename = "ref", default)]
    pub git_ref: String,
    /// The user who triggered the job.
    #[serde(default)]
    pub user: Option<CiJobUser>,
    pub pipeline: CiPipeline,
}

/// The user of a CI job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CiJobUser {
    pub id: i64,
    pub username: String,
}

/// A job's pipeline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CiPipeline {
    pub id: i64,
    /// The project the pipeline (and so the job) runs for.
    pub project_id: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct GitLabVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl GitLabVersion {
    /// Parses the leading `major.minor[.patch]` of a version string; `None` if it has none.
    pub fn parse(s: &str) -> Option<Self> {
        let core = s.split(['-', '+', ' ']).next().unwrap_or(s);
        let mut parts = core.split('.').map(|p| p.parse::<u32>());
        let major = parts.next()?.ok()?;
        let minor = parts.next()?.ok()?;
        let patch = parts.next().and_then(Result::ok).unwrap_or(0);
        Some(Self {
            major,
            minor,
            patch,
        })
    }

    const fn at_least(self, major: u32, minor: u32) -> bool {
        self.major > major || (self.major == major && self.minor >= minor)
    }

    /// Keyset pagination on `GET /projects` (GitLab 13.0+).
    pub const fn supports_keyset_projects(self) -> bool {
        self.at_least(13, 0)
    }

    /// `GET /personal_access_tokens/self` (GitLab 15.5+).
    pub const fn supports_token_self(self) -> bool {
        self.at_least(15, 5)
    }

    /// `POST /projects/:id/fork` takes `namespace_path` (GitLab 14.0+; `namespace` before).
    pub const fn supports_fork_namespace_path(self) -> bool {
        self.at_least(14, 0)
    }

    /// Draft merge requests use the `Draft:` prefix instead of `WIP:` (GitLab 14.0+).
    pub const fn uses_draft_prefix(self) -> bool {
        self.at_least(14, 0)
    }
}

impl std::fmt::Display for GitLabVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        let v = GitLabVersion::parse("16.5.1-ee").unwrap();
        assert_eq!((v.major, v.minor, v.patch), (16, 5, 1));
        assert!(v.supports_keyset_projects() && v.supports_token_self() && v.uses_draft_prefix());
        let old = GitLabVersion::parse("12.10-pre").unwrap();
        assert!(!old.supports_keyset_projects() && !old.uses_draft_prefix());
        assert!(
            !GitLabVersion::parse("15.4.0")
                .unwrap()
                .supports_token_self()
        );
        assert!(GitLabVersion::parse("garbage").is_none());
        assert_eq!(v.to_string(), "16.5.1");
    }

    #[test]
    fn project_tolerates_missing_fields() {
        let p: Project = serde_json::from_str(
            r#"{"id": 1, "path_with_namespace": "a/b", "web_url": "https://x/a/b",
                "forked_from_project": null, "description": null}"#,
        )
        .unwrap();
        assert_eq!(p.visibility, "private");
        assert!(p.forked_from_project.is_none());
    }
}
