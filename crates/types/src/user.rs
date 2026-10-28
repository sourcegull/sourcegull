//! Users, emails, orgs, namespaces and settings subjects.

use crate::{OrgId, UserId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

/// A user account (`users` table). The password hash is deliberately absent:
/// it never leaves the authentication layer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "sqlx", derive(sqlx::FromRow))]
pub struct User {
    pub id: UserId,
    pub username: String,
    pub display_name: Option<String>,
    pub avatar_url: Option<String>,
    pub site_admin: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub deleted_at: Option<DateTime<Utc>>,
    pub locked_until: Option<DateTime<Utc>>,
    pub failed_logins: i32,
}

/// An email address of a user (`user_emails` table).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "sqlx", derive(sqlx::FromRow))]
pub struct UserEmail {
    pub user_id: UserId,
    pub email: String,
    pub verified_at: Option<DateTime<Utc>>,
    pub is_primary: bool,
    pub created_at: DateTime<Utc>,
}

impl UserEmail {
    /// Whether the address has been verified.
    pub fn is_verified(&self) -> bool {
        self.verified_at.is_some()
    }
}

/// An organization (`orgs` table).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "sqlx", derive(sqlx::FromRow))]
pub struct Org {
    pub id: OrgId,
    pub name: String,
    pub display_name: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub deleted_at: Option<DateTime<Utc>>,
}

/// A member's role in an org. Stored as lowercase text (`member`, `admin`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[cfg_attr(feature = "sqlx", derive(sqlx::Type))]
#[cfg_attr(feature = "sqlx", sqlx(type_name = "text", rename_all = "lowercase"))]
pub enum OrgRole {
    #[default]
    Member,
    Admin,
}

impl OrgRole {
    /// The stored string form.
    pub const fn as_str(self) -> &'static str {
        match self {
            OrgRole::Member => "member",
            OrgRole::Admin => "admin",
        }
    }
}

impl fmt::Display for OrgRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Error returned when parsing an unknown [`OrgRole`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown org role {0:?}")]
pub struct ParseOrgRoleError(pub String);

impl FromStr for OrgRole {
    type Err = ParseOrgRoleError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "member" => Ok(OrgRole::Member),
            "admin" => Ok(OrgRole::Admin),
            other => Err(ParseOrgRoleError(other.to_owned())),
        }
    }
}

/// A membership row of an org (`org_members` table).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "sqlx", derive(sqlx::FromRow))]
pub struct OrgMember {
    pub org_id: OrgId,
    pub user_id: UserId,
    pub role: OrgRole,
    pub created_at: DateTime<Utc>,
}

/// The owner of namespaced things (bulk changes, saved searches, notebooks…).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", content = "id", rename_all = "lowercase")]
pub enum Namespace {
    User(UserId),
    Org(OrgId),
}

impl Namespace {
    /// The user id when this is a user namespace.
    pub fn user_id(self) -> Option<UserId> {
        match self {
            Namespace::User(id) => Some(id),
            Namespace::Org(_) => None,
        }
    }

    /// The org id when this is an org namespace.
    pub fn org_id(self) -> Option<OrgId> {
        match self {
            Namespace::Org(id) => Some(id),
            Namespace::User(_) => None,
        }
    }
}

impl From<UserId> for Namespace {
    fn from(id: UserId) -> Self {
        Namespace::User(id)
    }
}

impl From<OrgId> for Namespace {
    fn from(id: OrgId) -> Self {
        Namespace::Org(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn org_role_roundtrip() {
        for r in [OrgRole::Member, OrgRole::Admin] {
            assert_eq!(r.as_str().parse::<OrgRole>(), Ok(r));
        }
        assert!("owner".parse::<OrgRole>().is_err());
    }

    #[test]
    fn namespace_serde() {
        let ns = Namespace::Org(OrgId(3));
        let s = serde_json::to_string(&ns).unwrap();
        assert_eq!(s, r#"{"type":"org","id":3}"#);
        assert_eq!(serde_json::from_str::<Namespace>(&s).unwrap(), ns);
        assert_eq!(ns.org_id(), Some(OrgId(3)));
        assert_eq!(ns.user_id(), None);
    }
}

/// Whose settings a `settings` row belongs to, in cascade order
/// (global < org < user).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", content = "id", rename_all = "lowercase")]
pub enum SettingsSubject {
    Global,
    Org(OrgId),
    User(UserId),
}

impl SettingsSubject {
    /// The stored `settings.subject_type` value.
    pub const fn subject_type(self) -> &'static str {
        match self {
            SettingsSubject::Global => "global",
            SettingsSubject::Org(_) => "org",
            SettingsSubject::User(_) => "user",
        }
    }
}
