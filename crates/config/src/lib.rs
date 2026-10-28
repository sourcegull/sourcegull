//! Configuration for Sourcegull.
//!
//! - [`parse_jsonc`]: JSONC (comments, trailing commas) parsing with line/column errors.
//! - [`SiteConfig`], [`site_config_schema`], [`validate`]: the typed site config, its
//!   generated JSON Schema and validation (errors carry JSON pointers).
//! - [`ConfigHandle`]: lock-free, versioned hot config with change notifications.
//! - [`SettingsCascade`], [`Settings`]: global → org → user settings merging.
//! - [`SITE_CONFIG_SECTIONS`], [`organize_site_config`]: the sections site config keys are
//!   grouped in, and reordering a document by section.
//! - [`RolloutWindows`]: bulk changes rollout windows (`batchChanges.rolloutWindows`).
//! - [`FeatureFlag`], [`evaluate`]: boolean and rollout feature flags with overrides.
//! - [`secrets`]: redacting and restoring secrets in code host connection configurations.
//! - [`refs`]: `${env:NAME}` and `${file:/path}` references in configuration files.
//! - [`resources`]: the resources file (`SOURCEGULL_RESOURCES_FILE`).
//!
//! Storage and `NOTIFY`-driven reloads live outside this crate.

mod duration;
mod flags;
mod handle;
mod jsonc;
mod organize;
pub mod refs;
pub mod resources;
mod rollout;
pub mod secrets;
mod sections;
mod settings;
mod site;
mod validate;

pub use duration::{ConfigDuration, ParseDurationError};
pub use flags::{FULL_ROLLOUT, FeatureFlag, FlagKind, FlagOverrides, evaluate, rollout_bucket};
pub use handle::{ConfigHandle, SiteConfigHandle, Versioned};
pub use jsonc::{JsoncError, parse_jsonc, strip_jsonc};
pub use organize::{OrganizeError, organize_document, organize_site_config};
pub use rollout::{
    ROLLOUT_WINDOWS_KEY, RatePeriod, RolloutDay, RolloutRate, RolloutRateValue, RolloutSegment,
    RolloutWindow, RolloutWindowConfig, RolloutWindows,
};
pub use sections::{
    ConfigSection, OTHER_SECTION, SETTINGS_SECTIONS, SITE_CONFIG_SECTIONS, commented_reference,
    find_section, section_for_key, section_header,
};
pub use settings::{
    Notice, NoticeLocation, PatternType, Quicklink, Settings, SettingsCascade,
    default_settings_jsonc, merge_values, settings_schema,
};
pub use site::*;
pub use validate::{
    ConfigError, ConfigIssue, ValidatedConfig, remove_unsupported_auth_providers,
    site_config_schema, validate, validate_value,
};

/// The commented site config written on first boot.
pub fn default_site_config_jsonc() -> &'static str {
    include_str!("default_site_config.jsonc")
}
