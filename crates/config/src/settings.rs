//! User-facing settings and the settings cascade.
//!
//! Settings are JSONC documents attached to subjects: the global (site-wide) subject, each
//! org and each user. A viewer's effective settings are global → orgs → user, deep-merged.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::validate::{ConfigError, ConfigIssue};

/// Deep-merges `overlay` into `base`: objects merge key by key (recursively), while arrays,
/// scalars and `null` in `overlay` replace whatever `base` had.
pub fn merge_values(base: &mut Value, overlay: &Value) {
    match (base, overlay) {
        (Value::Object(b), Value::Object(o)) => {
            for (k, v) in o {
                match b.get_mut(k) {
                    Some(existing) => merge_values(existing, v),
                    None => {
                        b.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        (b, o) => *b = o.clone(),
    }
}

/// The settings documents that apply to one viewer, in cascade order.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SettingsCascade {
    /// Site-wide settings.
    pub global: Option<Value>,
    /// Settings of each org the viewer belongs to, lowest precedence first.
    pub orgs: Vec<Value>,
    /// The viewer's own settings (absent for anonymous viewers).
    pub user: Option<Value>,
}

impl SettingsCascade {
    /// Merges all subjects into one JSON object; later subjects override earlier ones.
    pub fn merged(&self) -> Value {
        let mut out = Value::Object(serde_json::Map::new());
        let layers = self
            .global
            .iter()
            .chain(self.orgs.iter())
            .chain(self.user.iter());
        for layer in layers {
            if layer.is_object() {
                merge_values(&mut out, layer);
            }
        }
        out
    }

    /// Merges and deserializes into typed [`Settings`].
    pub fn settings(&self) -> Result<Settings, ConfigError> {
        Settings::from_value(self.merged())
    }
}

/// Effective (merged) settings for a viewer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
#[schemars(title = "Settings")]
pub struct Settings {
    /// Pattern type used when a query has no `patternType:`.
    #[serde(rename = "search.defaultPatternType")]
    pub search_default_pattern_type: PatternType,
    /// Include forked repositories in search results by default.
    #[serde(rename = "search.includeForks")]
    pub search_include_forks: bool,
    /// Include archived repositories in search results by default.
    #[serde(rename = "search.includeArchived")]
    pub search_include_archived: bool,
    /// Links shown on the search home page.
    pub quicklinks: Vec<Quicklink>,
    /// Banners shown to users.
    pub notices: Vec<Notice>,
    /// All other settings, passed through untouched (e.g. feature toggles read by the UI).
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            search_default_pattern_type: PatternType::Keyword,
            search_include_forks: false,
            search_include_archived: false,
            quicklinks: Vec::new(),
            notices: Vec::new(),
            extra: BTreeMap::new(),
        }
    }
}

impl Settings {
    /// Deserializes settings from a (merged) JSON value.
    pub fn from_value(value: Value) -> Result<Self, ConfigError> {
        serde_json::from_value(value).map_err(|e| {
            ConfigError::Invalid(vec![ConfigIssue {
                pointer: String::new(),
                message: e.to_string(),
            }])
        })
    }
}

/// The JSON Schema of [`Settings`], for the settings editor. Properties carry their section
/// ([`crate::SETTINGS_SECTIONS`]) as `x-section`; the root lists them as `x-sections`.
pub fn settings_schema() -> Value {
    let mut schema =
        serde_json::to_value(schemars::schema_for!(Settings)).unwrap_or(Value::Bool(true));
    crate::sections::annotate_schema(&mut schema, crate::sections::SETTINGS_SECTIONS);
    schema
}

/// The document a settings editor starts from when a subject has no settings yet: every
/// setting, commented out with its default and description, grouped by section.
pub fn default_settings_jsonc() -> String {
    format!(
        "// Settings: JSON with comments. Uncomment a key to change it; the settings reference\n\
         // is at /help/settings. User settings override organization settings, which override\n\
         // global settings.\n{}",
        crate::sections::commented_reference(
            &settings_schema(),
            crate::sections::SETTINGS_SECTIONS
        )
    )
}

/// Search pattern type.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum PatternType {
    /// Terms are ANDed; quoted strings are literal.
    #[default]
    Keyword,
    /// Literal match, with `/.../` for regexp.
    Standard,
    /// Exact string match.
    Literal,
    /// Regular expression.
    Regexp,
    /// Structural match.
    Structural,
}

/// A link on the search home page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Quicklink {
    /// Link text.
    pub name: String,
    /// Target URL (absolute or relative to the instance).
    pub url: String,
    /// Optional tooltip.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// A banner shown to users.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Notice {
    /// Markdown message.
    pub message: String,
    /// Where the notice appears.
    pub location: NoticeLocation,
    /// Whether users can dismiss it.
    #[serde(default)]
    pub dismissible: bool,
}

/// Where a notice appears.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum NoticeLocation {
    /// At the top of every page.
    Top,
    /// On the search home page.
    Home,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn deep_merge_rules() {
        let mut base = json!({"a": {"x": 1, "y": [1, 2]}, "b": 1, "keep": true});
        merge_values(
            &mut base,
            &json!({"a": {"y": [3], "z": {"n": 1}}, "b": {"now": "object"}, "c": null}),
        );
        assert_eq!(
            base,
            json!({"a": {"x": 1, "y": [3], "z": {"n": 1}}, "b": {"now": "object"}, "keep": true, "c": null})
        );
    }

    #[test]
    fn cascade_order_global_orgs_user() {
        let cascade = SettingsCascade {
            global: Some(json!({
                "search.includeForks": true,
                "quicklinks": [{"name": "g", "url": "/g"}],
                "motd": {"a": 1, "b": 1},
            })),
            orgs: vec![
                json!({"search.defaultPatternType": "regexp", "motd": {"b": 2}}),
                json!({"search.defaultPatternType": "literal", "notices": [{"message": "hi", "location": "top"}]}),
            ],
            user: Some(json!({"search.includeForks": false, "motd": {"c": 3}})),
        };
        let s = cascade.settings().unwrap();
        assert_eq!(s.search_default_pattern_type, PatternType::Literal);
        assert!(!s.search_include_forks);
        assert!(!s.search_include_archived);
        assert_eq!(s.quicklinks.len(), 1);
        assert_eq!(s.notices[0].location, NoticeLocation::Top);
        assert_eq!(s.extra["motd"], json!({"a": 1, "b": 2, "c": 3}));
    }

    #[test]
    fn empty_cascade_gives_defaults() {
        let s = SettingsCascade::default().settings().unwrap();
        assert_eq!(s, Settings::default());
        assert_eq!(s.search_default_pattern_type, PatternType::Keyword);
    }

    #[test]
    fn bad_type_is_an_error() {
        let cascade = SettingsCascade {
            user: Some(json!({"search.defaultPatternType": "fuzzy"})),
            ..Default::default()
        };
        assert!(cascade.settings().is_err());
    }

    #[test]
    fn schema_has_settings_keys() {
        let schema = settings_schema();
        assert!(schema["properties"]["search.defaultPatternType"].is_object());
    }
}
