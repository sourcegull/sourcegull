//! Site config schema generation and validation.

use std::sync::LazyLock;

use serde_json::Value;

use crate::jsonc::{JsoncError, parse_jsonc};
use crate::site::SiteConfig;

/// A problem at a location in a config document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigIssue {
    /// JSON pointer (RFC 6901) to the offending value; `""` is the document root.
    pub pointer: String,
    /// Human-readable description.
    pub message: String,
}

impl std::fmt::Display for ConfigIssue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let p = if self.pointer.is_empty() {
            "/"
        } else {
            &self.pointer
        };
        write!(f, "{p}: {}", self.message)
    }
}

/// Why a config document was rejected.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ConfigError {
    /// The text is not valid JSONC.
    #[error("syntax error: {0}")]
    Syntax(#[from] JsoncError),
    /// The document does not match the schema.
    #[error("invalid config: {}", join_issues(.0))]
    Invalid(Vec<ConfigIssue>),
}

fn join_issues(issues: &[ConfigIssue]) -> String {
    issues
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

/// A successfully validated site config.
#[derive(Debug, Clone, PartialEq)]
pub struct ValidatedConfig {
    /// The typed config, with defaults filled in.
    pub config: SiteConfig,
    /// Non-fatal findings, such as unknown keys.
    pub warnings: Vec<ConfigIssue>,
}

static SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    let schema = schemars::schema_for!(SiteConfig);
    let mut schema = serde_json::to_value(schema).unwrap_or(Value::Bool(true));
    crate::sections::annotate_schema(&mut schema, crate::sections::SITE_CONFIG_SECTIONS);
    mark_secrets(&mut schema);
    schema
});

/// Property names (at any depth) whose values are credentials. The schema marks them
/// `"x-secret": true` so editors show them as password inputs.
pub const SECRET_PROPERTIES: &[&str] = &[
    "clientSecret",
    "password",
    "executors.accessToken",
    "scim.authToken",
];

/// Adds `"x-secret": true` to every property named in [`SECRET_PROPERTIES`], at any depth
/// (the root, `definitions`, and the variants of tagged enums).
fn mark_secrets(value: &mut Value) {
    match value {
        Value::Object(obj) => {
            if let Some(Value::Object(props)) = obj.get_mut("properties") {
                for (name, prop) in props.iter_mut() {
                    if SECRET_PROPERTIES.contains(&name.as_str())
                        && let Some(p) = prop.as_object_mut()
                    {
                        p.insert("x-secret".into(), Value::Bool(true));
                    }
                }
            }
            obj.values_mut().for_each(mark_secrets);
        }
        Value::Array(items) => items.iter_mut().for_each(mark_secrets),
        _ => {}
    }
}

static VALIDATOR: LazyLock<Result<jsonschema::Validator, String>> =
    LazyLock::new(|| jsonschema::draft7::new(&SCHEMA).map_err(|e| e.to_string()));

/// The JSON Schema (draft 7) of the site config, generated from [`SiteConfig`].
/// Each property carries its section as `x-section`, and the root lists the sections as
/// `x-sections`. Served to the in-UI editor at `/.api/site-config-schema`.
pub fn site_config_schema() -> Value {
    SCHEMA.clone()
}

/// The key of the auth provider list.
const AUTH_PROVIDERS_KEY: &str = "auth.providers";

/// Auth provider types that are recognized but not supported, with the reason given.
const UNSUPPORTED_AUTH_PROVIDERS: &[(&str, &str)] = &[(
    "saml",
    "SAML sign-in is not supported; use OpenID Connect (most SAML identity providers also offer it)",
)];

/// The entries of `auth.providers` whose type is unsupported, as `(index, issue)`.
fn unsupported_auth_providers(value: &Value) -> Vec<(usize, ConfigIssue)> {
    let Some(Value::Array(providers)) = value.get(AUTH_PROVIDERS_KEY) else {
        return Vec::new();
    };
    providers
        .iter()
        .enumerate()
        .filter_map(|(i, p)| {
            let ty = p.get("type")?.as_str()?;
            let (_, reason) = UNSUPPORTED_AUTH_PROVIDERS.iter().find(|(t, _)| *t == ty)?;
            Some((
                i,
                ConfigIssue {
                    pointer: format!("/{}/{i}", escape_pointer(AUTH_PROVIDERS_KEY)),
                    message: (*reason).to_owned(),
                },
            ))
        })
        .collect()
}

/// Removes the `auth.providers` entries of unsupported types (such as `saml`) from a
/// document and returns an issue for each, with its pointer in the original document.
///
/// [`validate`] rejects such entries. The site config loader removes them first, so a stored
/// config that has one still loads with its other settings and providers.
pub fn remove_unsupported_auth_providers(value: &mut Value) -> Vec<ConfigIssue> {
    let found = unsupported_auth_providers(value);
    if let Some(Value::Array(providers)) = value.get_mut(AUTH_PROVIDERS_KEY) {
        for (i, _) in found.iter().rev() {
            providers.remove(*i);
        }
    }
    found.into_iter().map(|(_, issue)| issue).collect()
}

/// Parses JSONC `text`, validates it against the site config schema and deserializes it.
///
/// Unknown keys are allowed (older or newer configs keep working) but reported in
/// [`ValidatedConfig::warnings`]. Auth providers of an unsupported type (`saml`) are errors.
pub fn validate(text: &str) -> Result<ValidatedConfig, ConfigError> {
    let value = parse_jsonc(text)?;
    validate_value(&value)
}

/// Like [`validate`], for an already-parsed document.
pub fn validate_value(value: &Value) -> Result<ValidatedConfig, ConfigError> {
    let validator = VALIDATOR.as_ref().map_err(|e| {
        ConfigError::Invalid(vec![ConfigIssue {
            pointer: String::new(),
            message: format!("internal error: site config schema does not compile: {e}"),
        }])
    })?;

    let unsupported: Vec<ConfigIssue> = unsupported_auth_providers(value)
        .into_iter()
        .map(|(_, issue)| issue)
        .collect();
    // Schema errors inside an unsupported provider only repeat that it matches no known type.
    let within_unsupported = |pointer: &str| {
        unsupported.iter().any(|u| {
            pointer
                .strip_prefix(u.pointer.as_str())
                .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
        })
    };
    let mut issues: Vec<ConfigIssue> = validator
        .iter_errors(value)
        .map(|e| ConfigIssue {
            pointer: e.instance_path.to_string(),
            message: e.to_string(),
        })
        .filter(|i| !within_unsupported(&i.pointer))
        .collect();
    issues.extend(unsupported.iter().cloned());
    if !issues.is_empty() {
        issues.sort_by(|a, b| a.pointer.cmp(&b.pointer));
        return Err(ConfigError::Invalid(issues));
    }

    let config: SiteConfig = serde_json::from_value(value.clone()).map_err(|e| {
        ConfigError::Invalid(vec![ConfigIssue {
            pointer: String::new(),
            message: e.to_string(),
        }])
    })?;

    if let Err(issues) =
        crate::rollout::RolloutWindows::parse(&config.batch_changes_rollout_windows)
    {
        let prefix = format!("/{}", escape_pointer(crate::rollout::ROLLOUT_WINDOWS_KEY));
        return Err(ConfigError::Invalid(
            issues
                .into_iter()
                .map(|i| ConfigIssue {
                    pointer: format!("{prefix}{}", i.pointer),
                    message: i.message,
                })
                .collect(),
        ));
    }

    let bad_globs: Vec<ConfigIssue> = config
        .search_index_always_index
        .iter()
        .flat_map(|(repo, patterns)| {
            patterns.iter().enumerate().filter_map(move |(i, p)| {
                let e = sourcegull_types::AlwaysIndex::new([p]).err()?;
                Some(ConfigIssue {
                    pointer: format!(
                        "/{}/{}/{i}",
                        escape_pointer(crate::site::ALWAYS_INDEX_KEY),
                        escape_pointer(repo)
                    ),
                    message: e.to_string(),
                })
            })
        })
        .collect();
    if !bad_globs.is_empty() {
        return Err(ConfigError::Invalid(bad_globs));
    }

    let mut warnings = Vec::new();
    unknown_keys(&SCHEMA, &SCHEMA, value, "", &mut warnings);
    legacy_experimental_features(value, &config, &mut warnings);
    Ok(ValidatedConfig { config, warnings })
}

/// Replaces the unknown-key warning of a deprecated `experimentalFeatures` object with
/// warnings about its entries: `gitServerPinnedRepos` (still read, see
/// [`SiteConfig::pinned_repos`]) gets a deprecation warning naming `repoNodes.pinnedRepos`,
/// each of its values that is not a string a warning that it is ignored, and any other entry
/// the normal unknown-key warning.
fn legacy_experimental_features(value: &Value, config: &SiteConfig, out: &mut Vec<ConfigIssue>) {
    use crate::site::{
        LEGACY_EXPERIMENTAL_FEATURES_KEY, LEGACY_PINNED_REPOS_KEY, PINNED_REPOS_KEY,
    };

    let Some(Value::Object(features)) = value.get(LEGACY_EXPERIMENTAL_FEATURES_KEY) else {
        return;
    };
    let root = format!("/{}", escape_pointer(LEGACY_EXPERIMENTAL_FEATURES_KEY));
    out.retain(|w| w.pointer != root);
    if features.is_empty() {
        out.push(ConfigIssue {
            pointer: root,
            message: format!(
                "{LEGACY_EXPERIMENTAL_FEATURES_KEY:?} is deprecated and unused; remove it"
            ),
        });
        return;
    }
    for (key, v) in features {
        let pointer = format!("{root}/{}", escape_pointer(key));
        if key != LEGACY_PINNED_REPOS_KEY {
            out.push(ConfigIssue {
                pointer,
                message: format!("unknown property {key:?}"),
            });
            continue;
        }
        let message = if !config.repo_nodes_pinned_repos.is_empty() {
            format!("deprecated and ignored because {PINNED_REPOS_KEY:?} is set; remove it")
        } else if v.is_object() {
            format!("deprecated: move these pins to {PINNED_REPOS_KEY:?}")
        } else {
            format!(
                "deprecated and ignored: not an object of repo name → repo node address; use {PINNED_REPOS_KEY:?}"
            )
        };
        out.push(ConfigIssue {
            pointer: pointer.clone(),
            message,
        });
        if let Value::Object(pins) = v {
            for (repo, node) in pins.iter().filter(|(_, node)| !node.is_string()) {
                out.push(ConfigIssue {
                    pointer: format!("{pointer}/{}", escape_pointer(repo)),
                    message: format!(
                        "the repo node address must be a string, not {node}; this pin is ignored"
                    ),
                });
            }
        }
    }
}

/// Collects keys of `value` that the schema does not declare, recursing into known
/// properties, arrays and (for `oneOf`/`anyOf`) the alternative that matches the value.
fn unknown_keys(
    root: &Value,
    schema: &Value,
    value: &Value,
    path: &str,
    out: &mut Vec<ConfigIssue>,
) {
    let schema = resolve(root, schema);
    let Some(obj) = schema.as_object() else {
        return;
    };

    if let Some(all) = obj.get("allOf").and_then(Value::as_array) {
        for s in all {
            unknown_keys(root, s, value, path, out);
        }
    }
    for key in ["oneOf", "anyOf"] {
        if let Some(alts) = obj.get(key).and_then(Value::as_array)
            && let Some(alt) = alts.iter().find(|a| plausibly_matches(root, a, value))
        {
            unknown_keys(root, alt, value, path, out);
        }
    }

    match value {
        Value::Object(map) => {
            if let Some(props) = obj.get("properties").and_then(Value::as_object) {
                for (k, v) in map {
                    let child = format!("{path}/{}", escape_pointer(k));
                    match props.get(k) {
                        Some(s) => unknown_keys(root, s, v, &child, out),
                        None => out.push(ConfigIssue {
                            pointer: child,
                            message: format!("unknown property {k:?}"),
                        }),
                    }
                }
            }
        }
        Value::Array(items) => {
            if let Some(item_schema) = obj.get("items") {
                for (i, v) in items.iter().enumerate() {
                    unknown_keys(root, item_schema, v, &format!("{path}/{i}"), out);
                }
            }
        }
        _ => {}
    }
}

/// A cheap check used to pick the `oneOf` alternative for a value: required keys are
/// present and `enum`-constrained properties (such as a `"type"` tag) match.
fn plausibly_matches(root: &Value, schema: &Value, value: &Value) -> bool {
    let schema = resolve(root, schema);
    let Some(s) = schema.as_object() else {
        return false;
    };
    let Value::Object(map) = value else {
        return !s.contains_key("properties");
    };
    if let Some(req) = s.get("required").and_then(Value::as_array)
        && req
            .iter()
            .filter_map(Value::as_str)
            .any(|k| !map.contains_key(k))
    {
        return false;
    }
    let Some(props) = s.get("properties").and_then(Value::as_object) else {
        return false;
    };
    props.iter().all(|(k, p)| {
        match (
            map.get(k),
            resolve(root, p).get("enum").and_then(Value::as_array),
        ) {
            (Some(v), Some(allowed)) => allowed.contains(v),
            _ => true,
        }
    })
}

/// Follows a local `$ref` (`#/definitions/...`) if present.
fn resolve<'a>(root: &'a Value, schema: &'a Value) -> &'a Value {
    let mut s = schema;
    for _ in 0..16 {
        let Some(r) = s.get("$ref").and_then(Value::as_str) else {
            break;
        };
        match r.strip_prefix('#').and_then(|p| root.pointer(p)) {
            Some(target) => s = target,
            None => break,
        }
    }
    s
}

fn escape_pointer(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}
