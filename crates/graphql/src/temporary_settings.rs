//! Temporary settings: small per-user UI state kept on the server:
//! `temporarySettings`, `overwriteTemporarySettings` and `editTemporarySettings`.
//! The UI keeps recent searches under `search.input.recentSearches`. Only signed-in users have
//! temporary settings; the UI keeps them in local storage for anonymous visitors.

use async_graphql::{Context, Object, Result};
use serde_json::Value;

use crate::EmptyResponse;
use crate::context::{codes, db_error, error, require_viewer, services};

/// Largest accepted contents, in bytes, so the table stays small UI state.
const MAX_CONTENTS_BYTES: usize = 512 * 1024;

/// A user's temporary settings.
pub struct TemporarySettingsNode(String);

#[Object(name = "TemporarySettings")]
impl TemporarySettingsNode {
    /// A JSON object of temporary settings.
    async fn contents(&self) -> &str {
        &self.0
    }
}

/// Parses `text` as a JSON object (size checked).
fn parse_object(text: &str, what: &str) -> Result<Value> {
    if text.len() > MAX_CONTENTS_BYTES {
        return Err(error(
            codes::INVALID_INPUT,
            format!("{what} is larger than {MAX_CONTENTS_BYTES} bytes."),
        ));
    }
    match serde_json::from_str::<Value>(text) {
        Ok(v @ Value::Object(_)) => Ok(v),
        _ => Err(error(
            codes::INVALID_INPUT,
            format!("{what} must be a JSON object."),
        )),
    }
}

#[derive(Default)]
pub struct TemporarySettingsQuery;

#[Object]
impl TemporarySettingsQuery {
    /// The signed-in viewer's temporary settings (an empty object if none were saved).
    async fn temporary_settings(&self, ctx: &Context<'_>) -> Result<TemporarySettingsNode> {
        let viewer = require_viewer(ctx)?;
        let value = services(ctx)?
            .db
            .temporary_settings()
            .get(viewer.user_id)
            .await
            .map_err(db_error)?;
        Ok(TemporarySettingsNode(value.to_string()))
    }
}

#[derive(Default)]
pub struct TemporarySettingsMutation;

#[Object]
impl TemporarySettingsMutation {
    /// Replaces the viewer's temporary settings with `contents`, a JSON object.
    async fn overwrite_temporary_settings(
        &self,
        ctx: &Context<'_>,
        contents: String,
    ) -> Result<EmptyResponse> {
        let viewer = require_viewer(ctx)?;
        let value = parse_object(&contents, "contents")?;
        services(ctx)?
            .db
            .temporary_settings()
            .overwrite(viewer.user_id, &value)
            .await
            .map_err(db_error)?;
        Ok(EmptyResponse)
    }

    /// Merges the top-level keys of `settingsToEdit`, a JSON object, into the viewer's
    /// temporary settings; keys not named keep their values.
    async fn edit_temporary_settings(
        &self,
        ctx: &Context<'_>,
        settings_to_edit: String,
    ) -> Result<EmptyResponse> {
        let viewer = require_viewer(ctx)?;
        let value = parse_object(&settings_to_edit, "settingsToEdit")?;
        services(ctx)?
            .db
            .temporary_settings()
            .edit(viewer.user_id, &value)
            .await
            .map_err(db_error)?;
        Ok(EmptyResponse)
    }
}
