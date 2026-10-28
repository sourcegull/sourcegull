//! Code host connections and repositories.

use crate::{ExternalServiceId, RepoId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

/// Error returned when parsing one of the enums in this module fails.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown {kind} {value:?}")]
pub struct ParseEnumError {
    /// The enum name, e.g. `ExternalServiceKind`.
    pub kind: &'static str,
    /// The rejected input.
    pub value: String,
}

/// The kind of a code host connection (`external_services.kind`). Stored and shown in upper
/// case (`GITLAB`). Hosts are added here together with their `codehost-*` crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
#[cfg_attr(feature = "sqlx", derive(sqlx::Type))]
#[cfg_attr(feature = "sqlx", sqlx(type_name = "text", rename_all = "UPPERCASE"))]
pub enum ExternalServiceKind {
    /// gitlab.com or a self-managed GitLab.
    GitLab,
    /// github.com or GitHub Enterprise Server.
    GitHub,
    /// A Gitea instance.
    Gitea,
}

impl ExternalServiceKind {
    /// Every kind.
    pub const ALL: [ExternalServiceKind; 3] = [
        ExternalServiceKind::GitLab,
        ExternalServiceKind::GitHub,
        ExternalServiceKind::Gitea,
    ];

    /// The stored form (`GITLAB`).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::GitLab => "GITLAB",
            Self::GitHub => "GITHUB",
            Self::Gitea => "GITEA",
        }
    }

    /// The lower-case service type stored on repos (`repo.external_service_type`), e.g.
    /// `gitlab`.
    pub const fn service_type(self) -> &'static str {
        match self {
            Self::GitLab => "gitlab",
            Self::GitHub => "github",
            Self::Gitea => "gitea",
        }
    }

    /// The code host's product name, as shown to users (`GitLab`).
    pub const fn name(self) -> &'static str {
        match self {
            Self::GitLab => "GitLab",
            Self::GitHub => "GitHub",
            Self::Gitea => "Gitea",
        }
    }
}

impl fmt::Display for ExternalServiceKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ExternalServiceKind {
    type Err = ParseEnumError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|k| k.as_str().eq_ignore_ascii_case(s))
            .ok_or_else(|| ParseEnumError {
                kind: "ExternalServiceKind",
                value: s.to_owned(),
            })
    }
}

/// A code host connection (`external_services` row) without its configuration, which is
/// stored encrypted and only decrypted on demand.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "sqlx", derive(sqlx::FromRow))]
pub struct ExternalService {
    pub id: ExternalServiceId,
    pub kind: ExternalServiceKind,
    pub display_name: String,
    /// Identifies the connection in `/.api/webhooks/{uuid}`.
    pub webhook_uuid: String,
    /// When the last discovery sync finished (successfully or not).
    pub last_sync_at: Option<DateTime<Utc>>,
    /// When the next discovery sync is due.
    pub next_sync_at: Option<DateTime<Utc>>,
    /// When the last full (non-incremental) listing finished successfully.
    pub last_full_sync_at: Option<DateTime<Utc>>,
    /// The incremental listing watermark: the newest `last_activity_at` seen.
    pub sync_watermark: Option<DateTime<Utc>>,
    /// The error of the last sync, if it failed.
    pub last_sync_error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub deleted_at: Option<DateTime<Utc>>,
}

/// Project visibility on the code host (`repo.visibility`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[cfg_attr(feature = "sqlx", derive(sqlx::Type))]
#[cfg_attr(feature = "sqlx", sqlx(type_name = "text", rename_all = "lowercase"))]
pub enum Visibility {
    /// Visible to everyone, including anonymous users.
    #[default]
    Public,
    /// Visible to every signed-in user of the code host (GitLab "internal").
    Internal,
    /// Visible to members only.
    Private,
}

impl Visibility {
    /// The stored form.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Internal => "internal",
            Self::Private => "private",
        }
    }
}

impl FromStr for Visibility {
    type Err = ParseEnumError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "public" => Ok(Self::Public),
            "internal" => Ok(Self::Internal),
            "private" => Ok(Self::Private),
            _ => Err(ParseEnumError {
                kind: "Visibility",
                value: s.to_owned(),
            }),
        }
    }
}

/// Identifies a repository on its code host. `(service_type, service_id, id)` is stable across
/// renames and transfers, so it is what discovery matches on.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ExternalRepoSpec {
    /// The host's id for the repository (the GitLab project id, as a string).
    pub id: String,
    /// The lower-case host type, e.g. `gitlab`.
    pub service_type: String,
    /// The host instance: its normalized base URL with a trailing slash, e.g.
    /// `https://gitlab.com/`.
    pub service_id: String,
}

/// A repository (`repo` row).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "sqlx", derive(sqlx::FromRow))]
pub struct Repo {
    pub id: RepoId,
    /// The unique name in Sourcegull, e.g. `gitlab.com/group/project`.
    pub name: String,
    /// `{host}/{pathWithNamespace}`, independent of `repositoryPathPattern`.
    pub uri: String,
    /// The repository's web page on the code host.
    pub url: String,
    pub description: String,
    pub fork: bool,
    pub archived: bool,
    /// Not visible to everyone on the code host (`internal` or `private`, unless
    /// `markInternalReposAsPublic`).
    pub private: bool,
    pub visibility: Visibility,
    pub stars: i32,
    pub topics: Vec<String>,
    pub default_branch: Option<String>,
    pub last_activity_at: Option<DateTime<Utc>>,
    pub external_id: String,
    pub external_service_type: String,
    pub external_service_id: String,
    /// The repo node this repo is pinned to; `None` means "use the hash assignment".
    pub shard: Option<String>,
    /// Why the repo is blocked from being served, if it is.
    pub blocked: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub deleted_at: Option<DateTime<Utc>>,
}

impl Repo {
    /// The repo's external spec.
    pub fn external_repo(&self) -> ExternalRepoSpec {
        ExternalRepoSpec {
            id: self.external_id.clone(),
            service_type: self.external_service_type.clone(),
            service_id: self.external_service_id.clone(),
        }
    }
}

/// Clone state of a repo on its repo node (`gitserver_repos.clone_status`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "sqlx", derive(sqlx::Type))]
#[cfg_attr(feature = "sqlx", sqlx(type_name = "text", rename_all = "snake_case"))]
pub enum CloneStatus {
    /// Not on disk yet.
    #[default]
    NotCloned,
    /// The initial clone is running.
    Cloning,
    /// On disk; fetches keep it current.
    Cloned,
}

impl CloneStatus {
    /// The stored form.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotCloned => "not_cloned",
            Self::Cloning => "cloning",
            Self::Cloned => "cloned",
        }
    }
}

impl FromStr for CloneStatus {
    type Err = ParseEnumError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "not_cloned" => Ok(Self::NotCloned),
            "cloning" => Ok(Self::Cloning),
            "cloned" => Ok(Self::Cloned),
            _ => Err(ParseEnumError {
                kind: "CloneStatus",
                value: s.to_owned(),
            }),
        }
    }
}

/// Why a code host project has no git repository the repo node can fetch
/// (`gitserver_repos.remote_unavailable`). None of these is a failed clone: the repo is
/// recorded as cloned with an empty local copy, and the next scheduled fetch picks up the
/// repository once it becomes available.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "sqlx", derive(sqlx::Type))]
#[cfg_attr(feature = "sqlx", sqlx(type_name = "text", rename_all = "snake_case"))]
pub enum RemoteUnavailable {
    /// The project has no repository yet (GitLab `empty_repo`: never pushed to).
    Missing,
    /// The project's repository feature is switched off (GitLab
    /// `repository_access_level: disabled`).
    Disabled,
    /// The code host token may see the project but not download its code (GitLab: below
    /// Reporter on a private project, "You are not allowed to download code").
    NoCodeAccess,
}

impl RemoteUnavailable {
    /// The stored form.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Disabled => "disabled",
            Self::NoCodeAccess => "no_code_access",
        }
    }

    /// Whether the code host has no repository to fetch at all (missing or disabled), as
    /// opposed to one the token cannot read.
    pub const fn no_repository(self) -> bool {
        matches!(self, Self::Missing | Self::Disabled)
    }
}

impl FromStr for RemoteUnavailable {
    type Err = ParseEnumError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "missing" => Ok(Self::Missing),
            "disabled" => Ok(Self::Disabled),
            "no_code_access" => Ok(Self::NoCodeAccess),
            _ => Err(ParseEnumError {
                kind: "RemoteUnavailable",
                value: s.to_owned(),
            }),
        }
    }
}

/// The repo node's view of a repo (`gitserver_repos` row), maintained by the repo node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "sqlx", derive(sqlx::FromRow))]
pub struct GitserverRepo {
    pub repo_id: RepoId,
    pub clone_status: CloneStatus,
    /// Free-form progress line of a running clone.
    pub cloning_progress: Option<String>,
    /// When the last fetch (or clone) finished successfully.
    pub last_fetched: Option<DateTime<Utc>>,
    /// When the last fetch changed any ref.
    pub last_changed: Option<DateTime<Utc>>,
    /// The error of the last clone/fetch, cleared by the next success.
    pub last_error: Option<String>,
    /// Size of the bare repo on disk.
    pub repo_size_bytes: Option<i64>,
    /// The object id `HEAD` resolves to after the last fetch.
    pub head_oid: Option<String>,
    /// The repo node holding the repo, as reported by that node.
    pub node: Option<String>,
    /// Set when the repo node found the repo corrupt (it will re-clone).
    pub corrupted_at: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
    /// Why the code host has no repository the repo node can fetch (none yet, disabled, or
    /// no code access for the token): the repo is recorded as cloned and empty, not as a
    /// failed clone.
    #[cfg_attr(feature = "sqlx", sqlx(default))]
    #[serde(default)]
    pub remote_unavailable: Option<RemoteUnavailable>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enum_round_trips() {
        assert_eq!(
            "gitlab".parse::<ExternalServiceKind>(),
            Ok(ExternalServiceKind::GitLab)
        );
        assert_eq!(ExternalServiceKind::GitLab.service_type(), "gitlab");
        assert_eq!(
            serde_json::to_string(&ExternalServiceKind::GitLab).unwrap(),
            "\"GITLAB\""
        );
        assert_eq!(
            "GitHub".parse::<ExternalServiceKind>(),
            Ok(ExternalServiceKind::GitHub)
        );
        assert_eq!(ExternalServiceKind::GitHub.service_type(), "github");
        assert_eq!(
            "gitea".parse::<ExternalServiceKind>(),
            Ok(ExternalServiceKind::Gitea)
        );
        assert_eq!(ExternalServiceKind::Gitea.service_type(), "gitea");
        assert!("bitbucket".parse::<ExternalServiceKind>().is_err());
        for v in [
            Visibility::Public,
            Visibility::Internal,
            Visibility::Private,
        ] {
            assert_eq!(v.as_str().parse::<Visibility>(), Ok(v));
        }
        for c in [
            CloneStatus::NotCloned,
            CloneStatus::Cloning,
            CloneStatus::Cloned,
        ] {
            assert_eq!(c.as_str().parse::<CloneStatus>(), Ok(c));
        }
        for r in [
            RemoteUnavailable::Missing,
            RemoteUnavailable::Disabled,
            RemoteUnavailable::NoCodeAccess,
        ] {
            assert_eq!(r.as_str().parse::<RemoteUnavailable>(), Ok(r));
            assert_eq!(
                serde_json::to_string(&r).unwrap(),
                format!("\"{}\"", r.as_str())
            );
        }
    }
}
