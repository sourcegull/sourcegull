//! Secrets in JSONC configurations (code host connections and the site configuration):
//! replacing them with
//! [`REDACTED`] for display and restoring them when an admin sends the placeholder back,
//! keeping comments and formatting.
//!
//! Each configuration kind names its secret fields with a function returning their JSON
//! pointers in a parsed document.

use serde_json::Value;

use crate::{ConfigError, ConfigIssue, parse_jsonc};

/// Placeholder that replaces secrets when a configuration is shown to admins. Sending it back
/// unchanged on update keeps the stored secret.
pub const REDACTED: &str = "REDACTED";

/// The JSON pointers of the secret fields in a parsed configuration.
pub type SecretPointers = fn(&Value) -> Vec<String>;

fn issue(pointer: impl Into<String>, message: impl Into<String>) -> ConfigIssue {
    ConfigIssue {
        pointer: pointer.into(),
        message: message.into(),
    }
}

/// Replaces every secret in a JSONC configuration with [`REDACTED`], keeping comments and
/// formatting. Fails only if `text` is not valid JSONC.
pub fn redact_secrets(text: &str, pointers: SecretPointers) -> Result<String, ConfigError> {
    let value = parse_jsonc(text)?;
    let mut secrets: Vec<String> = pointers(&value)
        .iter()
        .filter_map(|p| value.pointer(p).and_then(Value::as_str))
        .filter(|s| !s.is_empty() && *s != REDACTED)
        .map(str::to_owned)
        .collect();
    // Longest first, so a secret that contains another is replaced whole.
    secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
    let mut out = text.to_owned();
    for s in secrets {
        let quoted = Value::String(s).to_string();
        out = out.replace(&quoted, &Value::String(REDACTED.to_owned()).to_string());
    }
    Ok(out)
}

/// What [`restore_secrets`] restored.
#[derive(Debug, Clone, PartialEq)]
pub struct Restored {
    /// The configuration with every placeholder replaced by the previous secret.
    pub value: Value,
    /// `new` with the placeholders replaced in place (comments and formatting kept); `None`
    /// when they could not be matched to their fields in the text.
    pub text: Option<String>,
    /// The pointers whose secret was restored.
    pub pointers: Vec<String>,
}

/// Restores secrets that an admin sent back as [`REDACTED`] from the previous configuration
/// `old`. `None` when `new` contains no placeholder. Fails when a placeholder has no
/// counterpart in `old`.
pub fn restore_secrets(
    new: &str,
    old: &str,
    pointers: SecretPointers,
) -> Result<Option<Restored>, ConfigError> {
    let mut value = parse_jsonc(new)?;
    let restored: Vec<String> = pointers(&value)
        .into_iter()
        .filter(|p| value.pointer(p).and_then(Value::as_str) == Some(REDACTED))
        .collect();
    if restored.is_empty() {
        return Ok(None);
    }
    let old = parse_jsonc(old)?;
    let mut missing = Vec::new();
    for p in &restored {
        match old.pointer(p).and_then(Value::as_str) {
            Some(secret) if secret != REDACTED => {
                if let Some(slot) = value.pointer_mut(p) {
                    *slot = Value::String(secret.to_owned());
                }
            }
            _ => missing.push(issue(
                p.clone(),
                "is REDACTED but there is no previous secret to keep; enter the secret",
            )),
        }
    }
    if !missing.is_empty() {
        return Err(ConfigError::Invalid(missing));
    }
    let text = splice_secrets(new, &value, pointers);
    Ok(Some(Restored {
        value,
        text,
        pointers: restored,
    }))
}

/// [`restore_secrets`] as text: `new` verbatim when it has no placeholder, else the restored
/// text, or the merged configuration as plain JSON when the placeholders cannot be replaced
/// in place.
pub fn unredact_secrets(
    new: &str,
    old: &str,
    pointers: SecretPointers,
) -> Result<String, ConfigError> {
    match restore_secrets(new, old, pointers)? {
        None => Ok(new.to_owned()),
        Some(Restored {
            text: Some(text), ..
        }) => Ok(text),
        Some(Restored { value, .. }) => to_pretty(&value),
    }
}

/// `value` as pretty-printed JSON.
pub fn to_pretty(value: &Value) -> Result<String, ConfigError> {
    serde_json::to_string_pretty(value)
        .map_err(|e| ConfigError::Invalid(vec![issue("", e.to_string())]))
}

/// Replaces each `"REDACTED"` literal of `text` that sits at a secret field with that field's
/// value in `merged`, keeping everything else (comments, formatting) as written. Returns `None`
/// when the result does not parse to `merged`.
fn splice_secrets(text: &str, merged: &Value, pointers: SecretPointers) -> Option<String> {
    let placeholder = Value::String(REDACTED.to_owned()).to_string();
    let marker = |i: usize| format!("\"__sourcegull_redacted_placeholder_{i}__\"");
    // Number every placeholder literal, then find which field each one landed in.
    let mut numbered = String::with_capacity(text.len());
    let mut rest = text;
    let mut count = 0;
    while let Some(at) = rest.find(&placeholder) {
        numbered.push_str(&rest[..at]);
        numbered.push_str(&marker(count));
        rest = &rest[at + placeholder.len()..];
        count += 1;
    }
    numbered.push_str(rest);
    let parsed = parse_jsonc(&numbered).ok()?;
    let mut out = numbered;
    for i in 0..count {
        let literal = marker(i);
        let value = pointers(&parsed)
            .into_iter()
            .find(|p| {
                parsed.pointer(p).and_then(Value::as_str) == Some(&literal[1..literal.len() - 1])
            })
            .and_then(|p| merged.pointer(&p).and_then(Value::as_str))
            .map_or_else(
                || placeholder.clone(),
                |s| Value::String(s.to_owned()).to_string(),
            );
        out = out.replacen(&literal, &value, 1);
    }
    (parse_jsonc(&out).ok()? == *merged).then_some(out)
}

/// The first `webhooks[].secret` of a configuration, if any.
pub fn webhook_secret(text: &str) -> Option<String> {
    parse_jsonc(text)
        .ok()?
        .pointer("/webhooks/0/secret")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// Sets the configuration's `webhooks` to just `[{"secret": secret}]`. When the configuration
/// has exactly one secret it is replaced in the text (comments and formatting kept); otherwise
/// the configuration is rewritten as plain JSON.
pub fn set_webhook_secret(text: &str, secret: &str) -> Result<String, ConfigError> {
    let mut value = parse_jsonc(text)?;
    let Some(obj) = value.as_object_mut() else {
        return Err(ConfigError::Invalid(vec![issue(
            "",
            "the configuration must be a JSON object",
        )]));
    };
    let hooks = obj.get("webhooks").and_then(Value::as_array);
    if let Some([only]) = hooks.map(Vec::as_slice)
        && let Some(old) = only.get("secret").and_then(Value::as_str)
        && !old.is_empty()
    {
        let quoted = Value::String(old.to_owned()).to_string();
        if text.matches(&quoted).count() == 1 {
            return Ok(text.replacen(&quoted, &Value::String(secret.to_owned()).to_string(), 1));
        }
    }
    obj.insert(
        "webhooks".to_owned(),
        serde_json::json!([{ "secret": secret }]),
    );
    to_pretty(&value)
}

/// The pointers of a top-level `token` and every `webhooks[].secret`.
pub fn token_and_webhook_secrets(value: &Value) -> Vec<String> {
    let mut out = Vec::new();
    if value.get("token").is_some_and(Value::is_string) {
        out.push("/token".to_owned());
    }
    out.extend(webhook_secret_pointers(value));
    out
}

/// The pointers of every `webhooks[].secret`.
pub fn webhook_secret_pointers(value: &Value) -> Vec<String> {
    let Some(hooks) = value.get("webhooks").and_then(Value::as_array) else {
        return Vec::new();
    };
    hooks
        .iter()
        .enumerate()
        .filter(|(_, h)| h.get("secret").is_some_and(Value::is_string))
        .map(|(i, _)| format!("/webhooks/{i}/secret"))
        .collect()
}

/// The site configuration keys whose string value is a credential.
const SITE_SECRET_KEYS: [&str; 3] = [
    "email.smtp/password",
    "executors.accessToken",
    "scim.authToken",
];

/// Whether a configuration string is a secret to hide: non-empty and not a `${env:…}` /
/// `${file:…}` reference, which names where the secret lives rather than holding it.
fn holds_secret(value: Option<&Value>) -> bool {
    value
        .and_then(Value::as_str)
        .is_some_and(|s| !s.is_empty() && !s.contains("${"))
}

/// The pointers of the site configuration's credentials: `email.smtp.password`,
/// `executors.accessToken`, `scim.authToken` and every `auth.providers[].clientSecret`.
/// These are the properties the schema marks secret (`SECRET_PROPERTIES`). Values that
/// are references (`${env:…}`, `${file:…}`) are left out.
pub fn site_secret_pointers(value: &Value) -> Vec<String> {
    let mut out: Vec<String> = SITE_SECRET_KEYS
        .iter()
        .map(|k| format!("/{k}"))
        .filter(|p| holds_secret(value.pointer(p)))
        .collect();
    if let Some(providers) = value.get("auth.providers").and_then(Value::as_array) {
        out.extend(
            providers
                .iter()
                .enumerate()
                .filter(|(_, p)| holds_secret(p.get("clientSecret")))
                .map(|(i, _)| format!("/auth.providers/{i}/clientSecret")),
        );
    }
    out
}

/// Replaces every credential in a site configuration (see [`site_secret_pointers`]) with
/// [`REDACTED`], keeping comments and formatting.
pub fn redact_site_config(text: &str) -> Result<String, ConfigError> {
    redact_secrets(text, site_secret_pointers)
}

/// Restores site configuration credentials that an admin sent back as [`REDACTED`] from the
/// stored configuration `old` (see [`unredact_secrets`]). An `auth.providers` entry takes the
/// secret of the stored provider with the same `type` and `clientID` (the one at the same
/// position when several match), so adding, removing or reordering providers keeps each
/// provider's own secret.
pub fn unredact_site_config(new: &str, old: &str) -> Result<String, ConfigError> {
    let new_value = parse_jsonc(new)?;
    let old_value = parse_jsonc(old).unwrap_or(Value::Null);
    let aligned = align_providers(&new_value, &old_value);
    unredact_secrets(new, &aligned.to_string(), site_secret_pointers)
}

/// `old` with `auth.providers` reordered to line up with `new`: entry `i` is the stored
/// provider matching `new`'s provider `i` (same `type` and `clientID`), or `{}` when none does.
fn align_providers(new: &Value, old: &Value) -> Value {
    let mut out = old.clone();
    let (Some(new_providers), Some(old_providers), Some(obj)) = (
        new.get("auth.providers").and_then(Value::as_array),
        old.get("auth.providers").and_then(Value::as_array),
        out.as_object_mut(),
    ) else {
        return out;
    };
    let identity = |p: &Value| (p.get("type").cloned(), p.get("clientID").cloned());
    let aligned: Vec<Value> = new_providers
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let id = identity(p);
            let matches: Vec<usize> = (0..old_providers.len())
                .filter(|&j| identity(&old_providers[j]) == id)
                .collect();
            let pick = if matches.contains(&i) {
                Some(i)
            } else {
                matches.first().copied()
            };
            pick.map_or_else(|| serde_json::json!({}), |j| old_providers[j].clone())
        })
        .collect();
    obj.insert("auth.providers".to_owned(), Value::Array(aligned));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_and_restore() {
        let text = r#"{
  // the token
  "url": "https://x",
  "token": "s3cret", /* "REDACTED" in a comment stays */
  "webhooks": [{"secret": "hook1"}]
}"#;
        let red = redact_secrets(text, token_and_webhook_secrets).unwrap();
        assert!(!red.contains("s3cret") && !red.contains("hook1"));
        assert_eq!(
            unredact_secrets(&red, text, token_and_webhook_secrets).unwrap(),
            text
        );
        assert_eq!(
            unredact_secrets(text, "{}", token_and_webhook_secrets).unwrap(),
            text
        );
        let added = r#"{"token": "REDACTED"}"#;
        assert!(unredact_secrets(added, "{}", token_and_webhook_secrets).is_err());
    }

    #[test]
    fn site_config_redact_and_restore() {
        let text = r#"{
  // mail
  "email.smtp": {"host": "smtp", "port": 587, "username": "u", "password": "smtp-pass"},
  "executors.accessToken": "executor-token-0123456789",
  "scim.authToken": "${env:SCIM_TOKEN}",
  "auth.providers": [
    {"type": "builtin"},
    {"type": "gitlab", "url": "https://gitlab.com", "clientID": "a", "clientSecret": "sec-a"},
    {"type": "github", "url": "https://github.com", "clientID": "b", "clientSecret": "sec-b"},
  ],
}"#;
        let red = redact_site_config(text).unwrap();
        for s in ["smtp-pass", "executor-token-0123456789", "sec-a", "sec-b"] {
            assert!(!red.contains(s), "{red}");
        }
        assert_eq!(red.matches("\"REDACTED\"").count(), 4);
        assert!(red.contains("${env:SCIM_TOKEN}") && red.contains("// mail"));
        assert_eq!(unredact_site_config(&red, text).unwrap(), text);

        // Removing the first OAuth provider keeps the second one's own secret.
        let removed = red.replace(
            r#"{"type": "gitlab", "url": "https://gitlab.com", "clientID": "a", "clientSecret": "REDACTED"},"#,
            "",
        );
        let restored = parse_jsonc(&unredact_site_config(&removed, text).unwrap()).unwrap();
        assert_eq!(restored["auth.providers"][1]["clientSecret"], "sec-b");

        // A new provider sent with the placeholder has no secret to keep.
        let changed = red.replace(r#""clientID": "b""#, r#""clientID": "c""#);
        let ConfigError::Invalid(issues) = unredact_site_config(&changed, text).unwrap_err() else {
            panic!("expected an invalid configuration");
        };
        assert_eq!(issues[0].pointer, "/auth.providers/2/clientSecret");
    }
}
