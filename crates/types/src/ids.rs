//! Id newtypes.

use std::fmt;
use std::str::FromStr;

/// Error returned when parsing an id from a string fails.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid {kind}: {source}")]
pub struct ParseIdError {
    /// The id type name, e.g. `UserId`.
    pub kind: &'static str,
    #[source]
    pub source: std::num::ParseIntError,
}

macro_rules! define_id {
    ($(#[$meta:meta])* $name:ident($inner:ty)) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        #[derive(serde::Serialize, serde::Deserialize)]
        #[serde(transparent)]
        #[cfg_attr(feature = "sqlx", derive(sqlx::Type))]
        #[cfg_attr(feature = "sqlx", sqlx(transparent))]
        pub struct $name(pub $inner);

        impl $name {
            /// Returns the raw integer value.
            pub const fn get(self) -> $inner {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&self.0, f)
            }
        }

        impl FromStr for $name {
            type Err = ParseIdError;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                s.parse::<$inner>().map(Self).map_err(|source| ParseIdError {
                    kind: stringify!($name),
                    source,
                })
            }
        }

        impl From<$inner> for $name {
            fn from(v: $inner) -> Self {
                Self(v)
            }
        }

        impl From<$name> for $inner {
            fn from(v: $name) -> Self {
                v.0
            }
        }
    };
}

define_id!(
    /// A user (`users.id`).
    UserId(i64)
);
define_id!(
    /// An organization (`orgs.id`).
    OrgId(i64)
);
define_id!(
    /// A repository (`repo.id`). `i32` so ids fit in roaring bitmaps.
    RepoId(i32)
);
define_id!(
    /// A personal access token (`access_tokens.id`).
    AccessTokenId(i64)
);
define_id!(
    /// A code host connection (`external_services.id`).
    ExternalServiceId(i64)
);
define_id!(
    /// A discovery sync run of a code host connection (`external_service_sync_jobs.id`).
    ExternalServiceSyncJobId(i64)
);
define_id!(
    /// A repo update (clone/fetch) request (`repo_update_jobs.id`).
    RepoUpdateJobId(i64)
);
define_id!(
    /// A linked external account (`user_external_accounts.id`).
    ExternalAccountId(i64)
);
define_id!(
    /// A team (`teams.id`).
    TeamId(i64)
);
define_id!(
    /// An RBAC role (`roles.id`).
    RoleId(i64)
);
define_id!(
    /// An RBAC permission (`permissions.id`).
    PermissionId(i64)
);
define_id!(
    /// An org invitation (`org_invitations.id`).
    OrgInvitationId(i64)
);
define_id!(
    /// A version of the site configuration (`site_config.id`).
    SiteConfigId(i64)
);
define_id!(
    /// A version of a settings subject (`settings.id`).
    SettingsId(i64)
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_parse_roundtrip() {
        let id = UserId(42);
        assert_eq!(id.to_string(), "42");
        assert_eq!("42".parse::<UserId>(), Ok(id));
        let err = "x".parse::<RepoId>().unwrap_err();
        assert_eq!(err.kind, "RepoId");
        assert!(err.to_string().starts_with("invalid RepoId"));
    }

    #[test]
    fn serde_is_transparent() {
        assert_eq!(serde_json::to_string(&RepoId(7)).unwrap(), "7");
        assert_eq!(serde_json::from_str::<OrgId>("9").unwrap(), OrgId(9));
    }
}
