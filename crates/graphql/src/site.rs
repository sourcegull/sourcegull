//! The site: `site { configuration }` and `updateSiteConfiguration`.
//!
//! Writes are validated with `sourcegull-config` (errors reject the write with `VALIDATION`;
//! warnings such as unknown keys are kept and reported as `validationMessages`), stored with
//! optimistic concurrency (`lastID` must be the latest version, else `CONFLICT`), and go live
//! in every process through the `NOTIFY site_config` reload.
//!
//! Reads show credentials (SMTP password, auth provider client secrets, executor and SCIM
//! tokens) as `REDACTED`; saving a document that still holds `REDACTED` keeps the stored
//! secret there.

use async_graphql::{Context, ID, Object, Result};
use serde_json::json;
use sourcegull_config::ConfigError;
use sourcegull_services::SiteConfigSource;
use sourcegull_services::relay::marshal_string_id;
use sourcegull_types::SiteConfigId;

use crate::audit::{self, events};
use crate::context::{Services, codes, db_error, error, require_site_admin, services};

/// The singleton site's ID (`Site:"site"`).
pub fn site_id() -> ID {
    ID(marshal_string_id("Site", "site"))
}

/// The site (singleton).
pub struct Site;

#[Object]
impl Site {
    /// The opaque node ID.
    pub async fn id(&self) -> ID {
        site_id()
    }

    /// The latest global settings.
    pub async fn latest_settings(
        &self,
        ctx: &Context<'_>,
    ) -> Result<Option<crate::settings::SettingsNode>> {
        crate::settings::latest(ctx, sourcegull_types::SettingsSubject::Global).await
    }

    /// The URL to the global settings.
    #[graphql(name = "settingsURL")]
    pub async fn settings_url(&self) -> Option<String> {
        Some("/site-admin/global-settings".to_owned())
    }

    /// Whether the viewer can change the global settings (site admins, unless they come from
    /// SOURCEGULL_GLOBAL_SETTINGS_FILE).
    pub async fn viewer_can_administer(&self, ctx: &Context<'_>) -> Result<bool> {
        if !crate::context::viewer(ctx).is_some_and(|a| a.site_admin) {
            return Ok(false);
        }
        Ok(!crate::declared::global_settings_from_file(services(ctx)?).await?)
    }

    /// The configuration files in use (SOURCEGULL_RESOURCES_FILE, SOURCEGULL_GLOBAL_SETTINGS_FILE) and
    /// how their last application went (site admins only).
    async fn declared_config_files(
        &self,
        ctx: &Context<'_>,
    ) -> Result<Vec<crate::declared::DeclaredConfigFile>> {
        require_site_admin(ctx)?;
        crate::declared::files(services(ctx)?).await
    }

    /// The global settings as a cascade of one subject.
    pub async fn settings_cascade(
        &self,
        ctx: &Context<'_>,
    ) -> Result<crate::settings::SettingsCascadeNode> {
        crate::settings::cascade(ctx, sourcegull_types::SettingsSubject::Global).await
    }

    /// Sourcegull's version.
    async fn product_version(&self) -> String {
        env!("CARGO_PKG_VERSION").to_owned()
    }

    /// The site configuration (site admins only).
    async fn configuration(&self, ctx: &Context<'_>) -> Result<SiteConfiguration> {
        require_site_admin(ctx)?;
        current_configuration(services(ctx)?).await
    }

    /// The settings document a settings editor starts from when a subject has none yet: every
    /// setting commented out with its default and description, grouped by section.
    async fn default_settings(&self) -> String {
        sourcegull_config::default_settings_jsonc()
    }

    /// `input` (a site configuration document) with its top-level keys ordered by section
    /// under `// ── Section ──` headers, comments kept with their keys (site admins
    /// only). Nothing is saved; the editor shows the result as unsaved changes.
    async fn organize_configuration(&self, ctx: &Context<'_>, input: String) -> Result<String> {
        require_site_admin(ctx)?;
        sourcegull_config::organize_site_config(&input).map_err(|e| {
            error(
                codes::VALIDATION,
                format!("The site configuration could not be organized: {e}"),
            )
        })
    }

    /// Usage analytics from the local event logs (site admins only).
    async fn analytics(
        &self,
        ctx: &Context<'_>,
    ) -> Result<sourcegull_analytics_graphql::Analytics> {
        require_site_admin(ctx)?;
        Ok(sourcegull_analytics_graphql::Analytics::new())
    }

    /// Daily, weekly and monthly active users, newest first (site admins only).
    async fn usage_statistics(
        &self,
        ctx: &Context<'_>,
        days: Option<i32>,
        weeks: Option<i32>,
        months: Option<i32>,
    ) -> Result<sourcegull_analytics_graphql::SiteUsageStatistics> {
        require_site_admin(ctx)?;
        sourcegull_analytics_graphql::usage_statistics(ctx, days, weeks, months).await
    }

    /// Users with their activity (site admins only). `query`, `username` and `email`
    /// match the username, display name or email.
    #[allow(clippy::too_many_arguments)]
    async fn users(
        &self,
        ctx: &Context<'_>,
        query: Option<String>,
        site_admin: Option<bool>,
        username: Option<String>,
        email: Option<String>,
        last_active_at: Option<sourcegull_analytics_graphql::SiteUsersDateRangeInput>,
        deleted_at: Option<sourcegull_analytics_graphql::SiteUsersDateRangeInput>,
        created_at: Option<sourcegull_analytics_graphql::SiteUsersDateRangeInput>,
        events_count: Option<sourcegull_analytics_graphql::SiteUsersNumberRangeInput>,
    ) -> Result<sourcegull_analytics_graphql::SiteUsers> {
        require_site_admin(ctx)?;
        Ok(sourcegull_analytics_graphql::SiteUsers::new(
            sourcegull_analytics_graphql::SiteUsersArgs {
                query,
                site_admin,
                username,
                email,
                last_active_at,
                deleted_at,
                created_at,
                events_count,
            },
        ))
    }

    /// Whether the database is ready for an upgrade: schema drift against the version it is
    /// at (site admins only).
    async fn upgrade_readiness(
        &self,
        ctx: &Context<'_>,
    ) -> Result<crate::upgrade_readiness::UpgradeReadiness> {
        crate::upgrade_readiness::upgrade_readiness(ctx).await
    }
}

/// One version of the site configuration.
pub struct SiteConfiguration {
    id: i32,
    /// The stored document, with its secrets.
    contents: String,
    /// `contents` with its secrets replaced by `REDACTED`.
    redacted: String,
    read_only: bool,
}

impl SiteConfiguration {
    fn new(id: i32, contents: String, read_only: bool) -> Result<Self> {
        let redacted = sourcegull_config::secrets::redact_site_config(&contents).map_err(|e| {
            tracing::error!(error = %e, "redacting the site config");
            error(
                codes::INTERNAL,
                "The site configuration could not be read: it is not valid JSONC.",
            )
        })?;
        Ok(Self {
            id,
            contents,
            redacted,
            read_only,
        })
    }
}

#[Object]
impl SiteConfiguration {
    /// The version ID; pass it back as `lastID` when saving.
    async fn id(&self) -> i32 {
        self.id
    }

    /// The configuration as stored (JSONC, comments preserved), with secrets replaced by
    /// `REDACTED`. Saving `REDACTED` back keeps the stored secret.
    async fn effective_contents(&self) -> &str {
        &self.redacted
    }

    /// Problems found when validating the stored configuration (e.g. unknown keys).
    async fn validation_messages(&self) -> Vec<String> {
        match sourcegull_config::validate(&self.contents) {
            Ok(v) => v.warnings.iter().map(ToString::to_string).collect(),
            Err(e) => error_messages(&e),
        }
    }

    /// Whether edits are rejected because the configuration comes from a file.
    async fn read_only(&self) -> bool {
        self.read_only
    }
}

fn error_messages(e: &ConfigError) -> Vec<String> {
    match e {
        ConfigError::Syntax(s) => vec![format!("syntax error: {s}")],
        ConfigError::Invalid(issues) => issues.iter().map(ToString::to_string).collect(),
    }
}

async fn current_configuration(svc: &Services) -> Result<SiteConfiguration> {
    match &svc.site_config_source {
        SiteConfigSource::File(path) => {
            let contents = tokio::fs::read_to_string(path).await.map_err(|e| {
                tracing::error!(path = %path.display(), error = %e, "reading the site config file");
                error(codes::INTERNAL, "Could not read the site config file.")
            })?;
            SiteConfiguration::new(version_id(svc.config.version())?, contents, true)
        }
        SiteConfigSource::Database => {
            let latest = svc.db.site_config().latest().await.map_err(db_error)?;
            match latest {
                Some(row) => SiteConfiguration::new(version_id(row.id.get())?, row.contents, false),
                None => SiteConfiguration::new(
                    0,
                    sourcegull_config::default_site_config_jsonc().to_owned(),
                    false,
                ),
            }
        }
    }
}

/// A `VALIDATION` error listing the problems of a rejected document.
fn invalid(e: &ConfigError) -> async_graphql::Error {
    let messages = error_messages(e);
    let message = format!(
        "The site configuration is invalid:\n{}",
        messages
            .iter()
            .map(|m| format!("- {m}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    async_graphql::ErrorExtensions::extend_with(async_graphql::Error::new(message), |_, ext| {
        ext.set("code", codes::VALIDATION);
        ext.set("messages", messages.clone());
    })
}

fn version_id(v: i64) -> Result<i32> {
    i32::try_from(v).map_err(|_| error(codes::INTERNAL, "Site config version out of range."))
}

/// Site queries.
#[derive(Default)]
pub struct SiteQuery;

#[Object]
impl SiteQuery {
    /// The site.
    async fn site(&self) -> Site {
        Site
    }
}

/// Site mutations.
#[derive(Default)]
pub struct SiteMutation;

#[Object]
impl SiteMutation {
    /// Saves a new site configuration based on version `lastID` (site admins only). A secret
    /// left as `REDACTED` keeps its stored value. Returns whether a server restart is needed
    /// for the change to take effect (never, currently).
    async fn update_site_configuration(
        &self,
        ctx: &Context<'_>,
        #[graphql(name = "lastID")] last_id: i32,
        input: String,
    ) -> Result<bool> {
        let actor = require_site_admin(ctx)?;
        let svc = services(ctx)?;
        if svc.site_config_source.is_read_only() {
            return Err(error(
                codes::READ_ONLY,
                "The site configuration is read-only: it is set from a file \
                 (SOURCEGULL_SITE_CONFIG_FILE).",
            ));
        }
        // `REDACTED` placeholders take the stored secrets.
        let stored = match svc.db.site_config().latest().await.map_err(db_error)? {
            Some(row) => row.contents,
            None => sourcegull_config::default_site_config_jsonc().to_owned(),
        };
        let input = sourcegull_config::secrets::unredact_site_config(&input, &stored)
            .map_err(|e| invalid(&e))?;
        if let Err(e) = sourcegull_config::validate(&input) {
            return Err(invalid(&e));
        }
        let expected = (last_id > 0).then(|| SiteConfigId(i64::from(last_id)));
        let row = svc
            .db
            .site_config()
            .create(&input, Some(actor.user_id), expected)
            .await
            .map_err(|e| match e {
                sourcegull_db::DbError::Conflict { .. } => error(
                    codes::CONFLICT,
                    "The site configuration was modified since lastID; reload it and \
                     apply your changes again.",
                ),
                other => db_error(other),
            })?;
        tracing::info!(
            version = row.id.get(),
            user_id = actor.user_id.get(),
            "site config saved"
        );
        audit::record(
            svc,
            events::SITE_CONFIG_UPDATED,
            Some(actor.user_id),
            json!({ "id": row.id, "lastID": last_id }),
        )
        .await;
        Ok(false)
    }
}
