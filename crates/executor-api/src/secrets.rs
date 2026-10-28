//! Checks of executor secrets, shared by the API and the resources file: keys are environment
//! variable names (`^[A-Z][A-Z0-9_]*$`), and a `DOCKER_AUTH_CONFIG` value must be a Docker
//! config with `auths` only (no credential helpers or stores).

use std::collections::BTreeMap;

use base64::Engine as _;
use serde::Deserialize;

/// The key whose value is validated as a Docker config.
pub const DOCKER_AUTH_CONFIG: &str = "DOCKER_AUTH_CONFIG";

/// Whether `key` is a valid environment variable name.
pub fn valid_key(key: &str) -> bool {
    let mut chars = key.chars();
    chars.next().is_some_and(|c| c.is_ascii_uppercase())
        && chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DockerAuthConfigInput {
    #[serde(default)]
    auths: Option<BTreeMap<String, DockerAuthInput>>,
    #[serde(default, rename = "credsStore")]
    creds_store: Option<String>,
    #[serde(default, rename = "credHelpers")]
    cred_helpers: Option<BTreeMap<String, String>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DockerAuthInput {
    #[serde(default)]
    auth: Option<String>,
}

/// Checks a secret value: not empty, and for `DOCKER_AUTH_CONFIG` a Docker config whose
/// `auths` entries are `base64(username:password)`.
pub fn validate_value(key: &str, value: &str) -> std::result::Result<(), String> {
    if value.is_empty() {
        return Err("value cannot be empty string".into());
    }
    if key != DOCKER_AUTH_CONFIG {
        return Ok(());
    }
    let config: DockerAuthConfigInput = serde_json::from_str(value)
        .map_err(|e| format!("failed to unmarshal docker auth config for validation: {e}"))?;
    if config.cred_helpers.is_some_and(|h| !h.is_empty()) {
        return Err("cannot use credential helpers in docker auth config set via secrets".into());
    }
    if config.creds_store.is_some_and(|s| !s.is_empty()) {
        return Err("cannot use credential stores in docker auth config set via secrets".into());
    }
    for (registry, auth) in config.auths.unwrap_or_default() {
        let decoded = auth
            .auth
            .as_deref()
            .map(|a| base64::engine::general_purpose::STANDARD.decode(a));
        let ok = matches!(decoded, Some(Ok(bytes)) if bytes.contains(&b':'));
        if !ok {
            return Err(format!(
                "invalid credential in auths section for {registry:?} format has to be \
                 base64(username:password)"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys() {
        assert!(valid_key("GITHUB_TOKEN"));
        assert!(valid_key("A1_"));
        assert!(!valid_key(""));
        assert!(!valid_key("1A"));
        assert!(!valid_key("lower"));
        assert!(!valid_key("WITH-DASH"));
        assert!(!valid_key("_X"));
    }

    #[test]
    fn values() {
        assert!(validate_value("X", "").is_err());
        assert!(validate_value("X", "anything").is_ok());
        let auth = base64::engine::general_purpose::STANDARD.encode("user:pass");
        let ok = format!(r#"{{"auths": {{"registry.example.com": {{"auth": "{auth}"}}}}}}"#);
        assert!(validate_value(DOCKER_AUTH_CONFIG, &ok).is_ok());
        assert!(validate_value(DOCKER_AUTH_CONFIG, "not json").is_err());
        assert!(validate_value(DOCKER_AUTH_CONFIG, r#"{"unknown": 1}"#).is_err());
        let no_colon = base64::engine::general_purpose::STANDARD.encode("userpass");
        let bad = format!(r#"{{"auths": {{"r": {{"auth": "{no_colon}"}}}}}}"#);
        assert!(validate_value(DOCKER_AUTH_CONFIG, &bad).is_err());
        assert!(
            validate_value(DOCKER_AUTH_CONFIG, r#"{"credHelpers": {"r": "ecr-login"}}"#)
                .unwrap_err()
                .contains("credential helpers")
        );
        assert!(
            validate_value(DOCKER_AUTH_CONFIG, r#"{"credsStore": "desktop"}"#)
                .unwrap_err()
                .contains("credential stores")
        );
    }
}
