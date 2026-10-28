//! Gitea REST API objects (the fields Sourcegull uses).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize};

/// A repository (`GET /repos/{owner}/{repo}`, and the items of repository lists).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Repository {
    pub id: i64,
    #[serde(default)]
    pub owner: Option<Owner>,
    /// `owner/name`.
    pub full_name: String,
    #[serde(default, deserialize_with = "null_default")]
    pub description: String,
    pub html_url: String,
    #[serde(default)]
    pub clone_url: String,
    #[serde(default)]
    pub ssh_url: String,
    #[serde(default, deserialize_with = "null_default")]
    pub default_branch: String,
    #[serde(default)]
    pub private: bool,
    /// Visible to every signed-in user of the instance.
    #[serde(default)]
    pub internal: bool,
    #[serde(default)]
    pub archived: bool,
    #[serde(default)]
    pub fork: bool,
    /// A pull mirror of another repository.
    #[serde(default)]
    pub mirror: bool,
    #[serde(default)]
    pub template: bool,
    /// The repository has no commits.
    #[serde(default)]
    pub empty: bool,
    #[serde(default)]
    pub stars_count: i64,
    #[serde(default)]
    pub forks_count: i64,
    #[serde(default, deserialize_with = "null_default")]
    pub topics: Vec<String>,
    #[serde(default)]
    pub updated_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub created_at: Option<DateTime<Utc>>,
    /// The token user's access.
    #[serde(default)]
    pub permissions: Option<RepoPermissions>,
}

/// The owner of a repository: a user or an organization.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Owner {
    #[serde(default)]
    pub id: i64,
    #[serde(default)]
    pub login: String,
    /// `public`, `limited` (signed-in users only) or `private` (members only).
    #[serde(default)]
    pub visibility: Option<String>,
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

/// Who can read a repository, from its own flags and its owner's visibility (a public
/// repository of a private organization is readable by the organization's members only).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoVisibility {
    Public,
    /// Every signed-in user.
    Internal,
    Private,
}

impl Repository {
    pub fn visibility(&self) -> RepoVisibility {
        let owner = self.owner.as_ref().and_then(|o| o.visibility.as_deref());
        if self.private || owner == Some("private") {
            RepoVisibility::Private
        } else if self.internal || owner == Some("limited") {
            RepoVisibility::Internal
        } else {
            RepoVisibility::Public
        }
    }
}

/// `GET /repos/search`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct RepositorySearch {
    #[serde(default, deserialize_with = "null_default")]
    pub data: Vec<Repository>,
}

/// `GET /user`, the fields Sourcegull reads.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct User {
    pub id: i64,
    pub login: String,
    #[serde(default)]
    pub is_admin: bool,
}

/// `GET /version`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Version {
    pub version: String,
}

/// Deserializes `null` as the type's default.
fn null_default<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repository_tolerates_missing_and_null_fields() {
        let r: Repository = serde_json::from_str(
            r#"{"id": 1, "full_name": "a/b", "html_url": "https://gitea.example.com/a/b",
                "private": true, "description": null, "topics": null}"#,
        )
        .unwrap();
        assert_eq!(r.visibility(), RepoVisibility::Private);
        assert!(r.topics.is_empty() && r.description.is_empty());
        assert!(!r.fork);
    }

    #[test]
    fn visibility_follows_the_owner() {
        let mut r: Repository = serde_json::from_str(
            r#"{"id": 1, "full_name": "a/b", "html_url": "x",
                "owner": {"login": "a", "visibility": "public"}}"#,
        )
        .unwrap();
        assert_eq!(r.visibility(), RepoVisibility::Public);
        r.internal = true;
        assert_eq!(r.visibility(), RepoVisibility::Internal);
        r.internal = false;
        r.owner.as_mut().unwrap().visibility = Some("limited".into());
        assert_eq!(r.visibility(), RepoVisibility::Internal);
        r.owner.as_mut().unwrap().visibility = Some("private".into());
        assert_eq!(r.visibility(), RepoVisibility::Private);
    }
}
