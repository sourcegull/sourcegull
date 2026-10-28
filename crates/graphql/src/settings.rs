//! Settings: the settings cascade (global → orgs → user) over GraphQL:
//! `SettingsSubject`, `Settings`, `SettingsCascade` and `settingsMutation {
//! overwriteSettings }`.
//!
//! Who may read and write a subject's settings is decided by [`sourcegull_services::settings`].

use async_graphql::{Context, ID, InputObject, Interface, Object, Result};
use chrono::{DateTime, Utc};
use serde_json::Value;
use sourcegull_services::relay::unmarshal_id;
use sourcegull_types::{OrgId, SettingsId, SettingsSubject, UserId};

use crate::EmptyResponse;
use crate::context::{access_error, codes, db_error, error, require_viewer, services, viewer};
use crate::orgs::{ORG_KIND, OrgNode};
use crate::site::Site;
use crate::users::{USER_KIND, UserNode};

/// Something with settings: the site (global settings), an organization or a user.
// Values are short-lived (one per resolved field).
#[allow(clippy::large_enum_variant)]
#[derive(Interface)]
#[graphql(
    name = "SettingsSubject",
    field(name = "id", ty = "ID", desc = "The opaque node ID."),
    field(
        name = "latest_settings",
        ty = "Option<SettingsNode>",
        desc = "The latest settings."
    ),
    field(
        name = "settingsURL",
        method = "settings_url",
        ty = "Option<String>",
        desc = "The URL to the settings."
    ),
    field(
        name = "viewer_can_administer",
        ty = "bool",
        desc = "Whether the viewer can modify the subject's settings."
    ),
    field(
        name = "settings_cascade",
        ty = "SettingsCascadeNode",
        desc = "All settings for this subject and the levels of the cascade (global > \
                organization > user) merged into the final settings."
    )
)]
pub enum SettingsSubjectNode {
    Site(Site),
    Org(OrgNode),
    User(UserNode),
}

async fn require_read(ctx: &Context<'_>, subject: SettingsSubject) -> Result<()> {
    sourcegull_services::settings::require_read(&services(ctx)?.db, viewer(ctx), subject)
        .await
        .map_err(access_error)
}

/// `latestSettings` of a subject (read access checked).
pub async fn latest(ctx: &Context<'_>, subject: SettingsSubject) -> Result<Option<SettingsNode>> {
    require_read(ctx, subject).await?;
    Ok(services(ctx)?
        .db
        .settings()
        .latest(subject)
        .await
        .map_err(db_error)?
        .map(SettingsNode))
}

/// `settingsCascade` of a subject (read access checked): the global settings, then for a user
/// their organizations, then the subject itself.
pub async fn cascade(ctx: &Context<'_>, subject: SettingsSubject) -> Result<SettingsCascadeNode> {
    require_read(ctx, subject).await?;
    let mut subjects = vec![SettingsSubject::Global];
    match subject {
        SettingsSubject::Global => {}
        SettingsSubject::Org(_) => subjects.push(subject),
        SettingsSubject::User(u) => {
            let orgs = services(ctx)?
                .db
                .orgs()
                .list_for_user(u)
                .await
                .map_err(db_error)?;
            subjects.extend(orgs.into_iter().map(|o| SettingsSubject::Org(o.id)));
            subjects.push(subject);
        }
    }
    let store = services(ctx)?.db.settings();
    let mut contents = Vec::with_capacity(subjects.len());
    for s in &subjects {
        contents.push(
            store
                .latest(*s)
                .await
                .map_err(db_error)?
                .map(|v| v.contents),
        );
    }
    Ok(SettingsCascadeNode { subjects, contents })
}

/// Loads the node of a subject; `None` for a deleted organization or user.
async fn load_subject(
    ctx: &Context<'_>,
    subject: SettingsSubject,
) -> Result<Option<SettingsSubjectNode>> {
    let db = &services(ctx)?.db;
    Ok(match subject {
        SettingsSubject::Global => Some(SettingsSubjectNode::Site(Site)),
        SettingsSubject::Org(id) => db
            .orgs()
            .get(id)
            .await
            .map_err(db_error)?
            .map(|o| SettingsSubjectNode::Org(OrgNode(o))),
        SettingsSubject::User(id) => db
            .users()
            .get_by_id(id)
            .await
            .map_err(db_error)?
            .map(|u| SettingsSubjectNode::User(UserNode(u))),
    })
}

/// A version of a subject's settings.
pub struct SettingsNode(pub sourcegull_db::Settings);

#[Object(name = "Settings")]
impl SettingsNode {
    /// The version ID; pass it as `lastID` when saving.
    async fn id(&self) -> i32 {
        i32::try_from(self.0.id.get()).unwrap_or(i32::MAX)
    }

    /// The subject these settings are for.
    async fn subject(&self, ctx: &Context<'_>) -> Result<SettingsSubjectNode> {
        load_subject(ctx, self.0.subject)
            .await?
            .ok_or_else(|| error(codes::NOT_FOUND, "The settings subject no longer exists."))
    }

    /// The author; null if unknown or deleted.
    async fn author(&self, ctx: &Context<'_>) -> Result<Option<UserNode>> {
        let Some(id) = self.0.author_user_id else {
            return Ok(None);
        };
        Ok(services(ctx)?
            .db
            .users()
            .get_by_id(id)
            .await
            .map_err(db_error)?
            .map(UserNode))
    }

    /// When this version was saved.
    async fn created_at(&self) -> DateTime<Utc> {
        self.0.created_at
    }

    /// The settings as stored (JSONC: comments and trailing commas allowed).
    async fn contents(&self) -> &str {
        &self.0.contents
    }

    /// Whether the settings come from a file and cannot be changed through the API (global
    /// settings with SOURCEGULL_GLOBAL_SETTINGS_FILE).
    async fn read_only(&self, ctx: &Context<'_>) -> Result<bool> {
        if self.0.subject != SettingsSubject::Global {
            return Ok(false);
        }
        crate::declared::global_settings_from_file(services(ctx)?).await
    }
}

/// The settings documents that apply to a subject, and their merge.
pub struct SettingsCascadeNode {
    subjects: Vec<SettingsSubject>,
    contents: Vec<Option<String>>,
}

#[Object(name = "SettingsCascade")]
impl SettingsCascadeNode {
    /// The subjects merged into the final settings, lowest precedence first (site, the
    /// user's organizations, the user).
    async fn subjects(&self, ctx: &Context<'_>) -> Result<Vec<SettingsSubjectNode>> {
        let mut out = Vec::new();
        for s in &self.subjects {
            if let Some(n) = load_subject(ctx, *s).await? {
                out.push(n);
            }
        }
        Ok(out)
    }

    /// The merged settings as JSON. Documents that do not parse are skipped.
    #[graphql(name = "final")]
    async fn final_settings(&self) -> String {
        let mut merged = Value::Object(serde_json::Map::new());
        for text in self.contents.iter().flatten() {
            if let Ok(v @ Value::Object(_)) = sourcegull_config::parse_jsonc(text) {
                sourcegull_config::merge_values(&mut merged, &v);
            }
        }
        merged.to_string()
    }
}

/// Parses a settings subject ID: the site, an organization or a user.
pub fn parse_subject_id(id: &str) -> Result<SettingsSubject> {
    let invalid = || {
        error(
            codes::INVALID_INPUT,
            format!("Invalid settings subject {id:?}."),
        )
    };
    let (kind, key) = unmarshal_id(id).ok_or_else(invalid)?;
    match kind.as_str() {
        "Site" => Ok(SettingsSubject::Global),
        ORG_KIND => key
            .parse()
            .map(|k| SettingsSubject::Org(OrgId(k)))
            .map_err(|_| invalid()),
        USER_KIND => key
            .parse()
            .map(|k| SettingsSubject::User(UserId(k)))
            .map_err(|_| invalid()),
        _ => Err(invalid()),
    }
}

/// The subject and version a settings mutation applies to.
#[derive(InputObject)]
pub struct SettingsMutationGroupInput {
    /// The subject whose settings to change (the site, an organization or a user).
    pub subject: ID,
    /// The ID of the settings version the client last saw, or null if none; a newer version
    /// makes the mutation fail (concurrent editors).
    #[graphql(name = "lastID")]
    pub last_id: Option<i32>,
}

/// Settings mutations of one subject.
pub struct SettingsMutation {
    subject: SettingsSubject,
    last_id: Option<SettingsId>,
}

/// The result of a settings mutation.
pub struct UpdateSettingsPayload;

#[Object]
impl UpdateSettingsPayload {
    /// Always an empty response.
    async fn empty(&self) -> Option<EmptyResponse> {
        Some(EmptyResponse)
    }
}

#[Object(name = "SettingsMutation")]
impl SettingsMutation {
    /// Replaces the subject's settings with `contents` (a JSON object; comments and trailing
    /// commas are kept).
    async fn overwrite_settings(
        &self,
        ctx: &Context<'_>,
        contents: String,
    ) -> Result<Option<UpdateSettingsPayload>> {
        let actor = require_viewer(ctx)?;
        let value = sourcegull_config::parse_jsonc(&contents)
            .map_err(|e| error(codes::VALIDATION, format!("Invalid settings: {e:?}")))?;
        if !value.is_object() {
            return Err(error(codes::VALIDATION, "Settings must be a JSON object."));
        }
        sourcegull_config::Settings::from_value(value)
            .map_err(|e| error(codes::VALIDATION, format!("Invalid settings: {e}")))?;
        services(ctx)?
            .db
            .settings()
            .create(self.subject, &contents, Some(actor.user_id), self.last_id)
            .await
            .map_err(|e| match e {
                sourcegull_db::DbError::Conflict { .. } => error(
                    codes::CONFLICT,
                    "The settings were changed by someone else; reload and try again.",
                ),
                other => db_error(other),
            })?;
        Ok(Some(UpdateSettingsPayload))
    }
}

#[derive(Default)]
pub struct SettingsQuery;

#[Object]
impl SettingsQuery {
    /// The settings cascade of the viewer (only the global settings when signed out).
    async fn viewer_settings(&self, ctx: &Context<'_>) -> Result<SettingsCascadeNode> {
        match viewer(ctx) {
            Some(a) => cascade(ctx, SettingsSubject::User(a.user_id)).await,
            None => cascade(ctx, SettingsSubject::Global).await,
        }
    }
}

#[derive(Default)]
pub struct SettingsMutationRoot;

#[Object]
impl SettingsMutationRoot {
    /// Settings mutations of a subject: the viewer must be allowed to change its settings.
    async fn settings_mutation(
        &self,
        ctx: &Context<'_>,
        input: SettingsMutationGroupInput,
    ) -> Result<Option<SettingsMutation>> {
        let actor = require_viewer(ctx)?;
        let subject = parse_subject_id(&input.subject)?;
        if load_subject(ctx, subject).await?.is_none() {
            return Err(error(codes::NOT_FOUND, "Settings subject not found."));
        }
        sourcegull_services::settings::require_write(&services(ctx)?.db, actor, subject)
            .await
            .map_err(access_error)?;
        if subject == SettingsSubject::Global {
            crate::declared::ensure_global_settings_editable(services(ctx)?).await?;
        }
        Ok(Some(SettingsMutation {
            subject,
            last_id: input.last_id.map(|i| SettingsId(i64::from(i))),
        }))
    }
}
