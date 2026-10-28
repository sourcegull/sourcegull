//! Objects owned by the configuration-as-code files are read-only through the API: the guards
//! here turn an ownership recorded by the reconciler ([`sourcegull_db::DeclaredStore`]) into a
//! `READ_ONLY` error, and `managedBy` fields show the owning entry.

use async_graphql::{Result, SimpleObject};
use chrono::{DateTime, Utc};
use sourcegull_db::{DeclaredSource, Ownership};

use crate::context::{Services, codes, db_error, error};

/// The section names of the resources file, as the guards name kinds.
pub mod kinds {
    pub const CONNECTIONS: &str = "connections";
    pub const SEARCH_CONTEXTS: &str = "searchContexts";
    pub const ROLES: &str = "roles";
    pub const SITE_ADMINS: &str = "siteAdmins";
    pub const OUTBOUND_WEBHOOKS: &str = "outboundWebhooks";
    pub const EXECUTOR_SECRETS: &str = "executorSecrets";
    pub const CODE_INTEL_POLICIES: &str = "codeIntelPolicies";
    pub const FEATURE_FLAGS: &str = "featureFlags";
}

/// The `kind[key]` label of the entry that owns an object, if any.
pub async fn managed_by(svc: &Services, kind: &str, object_id: &str) -> Result<Option<String>> {
    Ok(
        match svc
            .db
            .declared()
            .ownership(kind, object_id)
            .await
            .map_err(db_error)?
        {
            Some(Ownership::Declared { key }) => Some(format!("{kind}[{key}]")),
            Some(Ownership::Exclusive) => Some(kind.to_owned()),
            None => None,
        },
    )
}

/// Fails with `READ_ONLY` when the resources file owns the object.
pub async fn ensure_not_declared(svc: &Services, kind: &str, object_id: &str) -> Result<()> {
    match svc
        .db
        .declared()
        .ownership(kind, object_id)
        .await
        .map_err(db_error)?
    {
        None => Ok(()),
        Some(Ownership::Declared { key }) => Err(error(
            codes::READ_ONLY,
            format!(
                "This is managed by SOURCEGULL_RESOURCES_FILE ({kind}[{key}]); change it in the file."
            ),
        )),
        Some(Ownership::Exclusive) => Err(exclusive(kind)),
    }
}

/// Fails with `READ_ONLY` when the resources file owns every object of `kind`.
pub async fn ensure_creatable(svc: &Services, kind: &str) -> Result<()> {
    if svc
        .db
        .declared()
        .is_exclusive(kind)
        .await
        .map_err(db_error)?
    {
        return Err(exclusive(kind));
    }
    Ok(())
}

fn exclusive(kind: &str) -> async_graphql::Error {
    error(
        codes::READ_ONLY,
        format!(
            "{kind} are managed exclusively by SOURCEGULL_RESOURCES_FILE; add them to the file \
             instead."
        ),
    )
}

/// Fails with `READ_ONLY` when the global settings come from a file.
pub async fn ensure_global_settings_editable(svc: &Services) -> Result<()> {
    if svc
        .db
        .declared()
        .source(sourcegull_db::GLOBAL_SETTINGS_SOURCE)
        .await
        .map_err(db_error)?
        .is_some()
    {
        return Err(error(
            codes::READ_ONLY,
            "The global settings are read-only: they are set from a file \
             (SOURCEGULL_GLOBAL_SETTINGS_FILE).",
        ));
    }
    Ok(())
}

/// Whether the global settings come from a file.
pub async fn global_settings_from_file(svc: &Services) -> Result<bool> {
    Ok(svc
        .db
        .declared()
        .source(sourcegull_db::GLOBAL_SETTINGS_SOURCE)
        .await
        .map_err(db_error)?
        .is_some())
}

/// A configuration-as-code file in use and how its last application went.
#[derive(SimpleObject)]
pub struct DeclaredConfigFile {
    /// `resources` (SOURCEGULL_RESOURCES_FILE) or `global_settings` (SOURCEGULL_GLOBAL_SETTINGS_FILE).
    pub source: String,
    /// The path in the worker that applies it.
    pub path: String,
    /// The sections the resources file manages.
    pub kinds: Vec<String>,
    /// Whether the managed sections own every object of their kind.
    pub exclusive: bool,
    /// When the file's current contents were first applied.
    pub applied_at: Option<DateTime<Utc>>,
    /// Why (part of) the file could not be applied; the last applied state stays.
    pub error: Option<String>,
    pub error_at: Option<DateTime<Utc>>,
    /// When the worker last read the file.
    pub checked_at: DateTime<Utc>,
}

impl From<DeclaredSource> for DeclaredConfigFile {
    fn from(s: DeclaredSource) -> Self {
        Self {
            source: s.source,
            path: s.path,
            kinds: s.kinds,
            exclusive: s.exclusive,
            applied_at: s.applied_at,
            error: s.error,
            error_at: s.error_at,
            checked_at: s.checked_at,
        }
    }
}

/// The files in use (site admins only, checked by the caller).
pub async fn files(svc: &Services) -> Result<Vec<DeclaredConfigFile>> {
    Ok(svc
        .db
        .declared()
        .sources()
        .await
        .map_err(db_error)?
        .into_iter()
        .map(Into::into)
        .collect())
}

/// Site admin changes the resources file decides: with `siteAdmins` declared, a user with a
/// declared verified email cannot be demoted, and with `exclusive` nobody can be promoted or
/// demoted through the API.
pub async fn ensure_site_admin_change_allowed(
    svc: &Services,
    user: sourcegull_types::UserId,
    site_admin: bool,
) -> Result<()> {
    let declared = svc.db.declared();
    let Some(source) = declared
        .source(sourcegull_db::RESOURCES_SOURCE)
        .await
        .map_err(db_error)?
    else {
        return Ok(());
    };
    if !source.kinds.iter().any(|k| k == kinds::SITE_ADMINS) {
        return Ok(());
    }
    if source.exclusive {
        return Err(exclusive(kinds::SITE_ADMINS));
    }
    if !site_admin {
        let emails = declared.site_admin_emails().await.map_err(db_error)?;
        let listed = declared
            .users_with_verified_emails(&emails)
            .await
            .map_err(db_error)?;
        if listed.contains(&user) {
            return Err(error(
                codes::READ_ONLY,
                "This user is a site admin through SOURCEGULL_RESOURCES_FILE (siteAdmins); remove \
                 their email there.",
            ));
        }
    }
    Ok(())
}

/// Site admins cannot verify or unverify a declared site admin address while the resources file
/// manages `siteAdmins`: verification is what makes its user site admin.
pub async fn ensure_email_verification_allowed(svc: &Services, email: &str) -> Result<()> {
    let declared = svc.db.declared();
    let managed = declared
        .source(sourcegull_db::RESOURCES_SOURCE)
        .await
        .map_err(db_error)?
        .is_some_and(|s| s.kinds.iter().any(|k| k == kinds::SITE_ADMINS));
    if !managed {
        return Ok(());
    }
    let emails = declared.site_admin_emails().await.map_err(db_error)?;
    if emails.contains(&email.trim().to_lowercase()) {
        return Err(error(
            codes::READ_ONLY,
            "This address is a site admin address in SOURCEGULL_RESOURCES_FILE (siteAdmins); only \
             its owner can verify it.",
        ));
    }
    Ok(())
}
