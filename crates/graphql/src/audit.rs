//! Security events recorded by GraphQL mutations, honouring
//! `log.securityEventLog.location`.

/// Event names.
pub mod events {
    /// The site configuration was saved.
    pub const SITE_CONFIG_UPDATED: &str = "SiteConfigUpdated";
    /// A user's site-admin flag was granted or revoked.
    pub const ROLE_CHANGE_GRANTED: &str = "RoleChangeGranted";
    /// A role assignment was removed.
    pub const ROLE_CHANGE_DENIED: &str = "RoleChangeDenied";
    /// A custom role was created.
    pub const ROLE_CREATED: &str = "RoleCreated";
    /// A custom role was deleted.
    pub const ROLE_DELETED: &str = "RoleDeleted";
    /// A role's permissions were changed.
    pub const ROLE_PERMISSIONS_CHANGED: &str = "RolePermissionsChanged";
    /// An organization was created.
    pub const ORG_CREATED: &str = "OrganizationCreated";
    /// An organization was deleted.
    pub const ORG_DELETED: &str = "OrganizationDeleted";
    /// A user was added to an organization without an invitation.
    pub const ORG_MEMBER_ADDED: &str = "OrganizationMemberAdded";
    /// A member was removed from an organization.
    pub const ORG_MEMBER_REMOVED: &str = "OrganizationMemberRemoved";
    /// A code host connection was added.
    pub const EXTERNAL_SERVICE_CREATED: &str = "ExternalServiceCreated";
    /// A code host connection was changed.
    pub const EXTERNAL_SERVICE_UPDATED: &str = "ExternalServiceUpdated";
    /// A code host connection was deleted.
    pub const EXTERNAL_SERVICE_DELETED: &str = "ExternalServiceDeleted";
    /// The users who may read a repository were replaced.
    pub const REPO_PERMISSIONS_SET: &str = "RepoPermissionsSet";
    /// Repositories were made readable by everyone, or restricted again.
    pub const REPO_PERMISSIONS_UNRESTRICTED: &str = "RepoPermissionsUnrestricted";
    /// An admin queued a code host permissions sync of a user or repository.
    pub const PERMISSIONS_SYNC_SCHEDULED: &str = "PermissionsSyncScheduled";
    pub const ACCESS_REQUEST_DECIDED: &str = "AccessRequestStatusChanged";
    /// A site admin deactivated a user.
    pub const USER_DEACTIVATED: &str = "UserDeactivated";
    /// A site admin reactivated a user.
    pub const USER_REACTIVATED: &str = "UserReactivated";
}

pub use sourcegull_graphql_core::audit::record;
