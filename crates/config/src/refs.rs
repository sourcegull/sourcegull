//! Secret references in configuration files: `${env:NAME}` and `${file:/path}` inside any
//! string value, replaced when the file is loaded, so the file can live in git while the
//! secrets come from the environment or mounted files (Kubernetes and Docker secrets).
//!
//! - `${env:NAME}`: the environment variable `NAME`. Unset is an error, never an empty string.
//! - `${file:/path}`: the file's contents without trailing newlines. Unreadable is an error.
//! - `$${` is a literal `${`. Any other `${…}` is left as it is.
//!
//! A resolved value that still looks like a secret injector reference (`vault:…`) is an error
//! too: the injector did not run. Error messages name the field and the variable or file,
//! never the value.

use serde_json::Value;

use crate::ConfigIssue;

/// Where references are looked up.
pub trait RefSource {
    /// The value of an environment variable, `None` when unset.
    fn env(&self, name: &str) -> Option<String>;
    /// The contents of a file.
    fn file(&self, path: &str) -> std::io::Result<String>;
}

/// The process environment and file system.
#[derive(Debug, Clone, Copy, Default)]
pub struct ProcessRefs;

impl RefSource for ProcessRefs {
    fn env(&self, name: &str) -> Option<String> {
        std::env::var_os(name).map(|v| v.to_string_lossy().into_owned())
    }

    fn file(&self, path: &str) -> std::io::Result<String> {
        std::fs::read_to_string(path)
    }
}

/// The values substituted by [`resolve_refs`], to keep them out of later messages.
#[derive(Default, Clone)]
pub struct Resolved {
    secrets: Vec<String>,
}

impl std::fmt::Debug for Resolved {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Resolved")
            .field("count", &self.secrets.len())
            .finish()
    }
}

impl Resolved {
    /// Whether any reference was replaced.
    pub fn is_empty(&self) -> bool {
        self.secrets.is_empty()
    }

    /// `message` with every substituted value replaced by `<secret>` (validation messages can
    /// quote the offending value).
    pub fn redact(&self, message: &str) -> String {
        let mut out = message.to_owned();
        for s in &self.secrets {
            // As is, and escaped the way `{:?}` and JSON quote it (a multi-line PEM, a value
            // with quotes).
            let debug = format!("{s:?}");
            let json = Value::String(s.clone()).to_string();
            let mut forms = vec![
                s.clone(),
                debug[1..debug.len() - 1].to_owned(),
                json[1..json.len() - 1].to_owned(),
            ];
            forms.sort_by_key(|f| std::cmp::Reverse(f.len()));
            forms.dedup();
            for f in forms.iter().filter(|f| !f.is_empty()) {
                out = out.replace(f.as_str(), "<secret>");
            }
        }
        out
    }

    /// [`redact`](Self::redact) for each issue's message.
    pub fn redact_issues(&self, issues: &mut [ConfigIssue]) {
        if self.secrets.is_empty() {
            return;
        }
        for i in issues {
            i.message = self.redact(&i.message);
        }
    }
}

/// Replaces the references in every string of `value` (object keys are left alone). On
/// failure, `value` may be partly resolved and the issues list every bad reference.
pub fn resolve_refs(
    value: &mut Value,
    source: &dyn RefSource,
) -> Result<Resolved, Vec<ConfigIssue>> {
    let mut resolved = Resolved::default();
    let mut issues = Vec::new();
    walk(
        value,
        &mut String::new(),
        source,
        &mut resolved,
        &mut issues,
    );
    if issues.is_empty() {
        Ok(resolved)
    } else {
        Err(issues)
    }
}

/// Whether any string in `value` contains a reference (or an escaped `$${`).
pub fn has_refs(value: &Value) -> bool {
    match value {
        Value::String(s) => s.contains("${"),
        Value::Array(items) => items.iter().any(has_refs),
        Value::Object(map) => map.values().any(has_refs),
        _ => false,
    }
}

fn walk(
    value: &mut Value,
    pointer: &mut String,
    source: &dyn RefSource,
    resolved: &mut Resolved,
    issues: &mut Vec<ConfigIssue>,
) {
    match value {
        Value::String(s) if s.contains("${") => match substitute(s, source, resolved) {
            Ok(new) => *s = new,
            Err(message) => issues.push(ConfigIssue {
                pointer: pointer.clone(),
                message,
            }),
        },
        Value::Array(items) => {
            for (i, item) in items.iter_mut().enumerate() {
                let len = pointer.len();
                pointer.push('/');
                pointer.push_str(&i.to_string());
                walk(item, pointer, source, resolved, issues);
                pointer.truncate(len);
            }
        }
        Value::Object(map) => {
            for (k, item) in map.iter_mut() {
                let len = pointer.len();
                pointer.push('/');
                pointer.push_str(&k.replace('~', "~0").replace('/', "~1"));
                walk(item, pointer, source, resolved, issues);
                pointer.truncate(len);
            }
        }
        _ => {}
    }
}

/// `text` with its references replaced.
fn substitute(
    text: &str,
    source: &dyn RefSource,
    resolved: &mut Resolved,
) -> Result<String, String> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('$') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        if let Some(after) = rest.strip_prefix("$${") {
            out.push_str("${");
            rest = after;
            continue;
        }
        let reference = rest
            .strip_prefix("${env:")
            .map(|r| (Kind::Env, r))
            .or_else(|| rest.strip_prefix("${file:").map(|r| (Kind::File, r)));
        let Some((kind, body)) = reference else {
            out.push('$');
            rest = &rest[1..];
            continue;
        };
        let end = body
            .find('}')
            .ok_or_else(|| format!("unterminated reference {:?}", truncate(rest)))?;
        let name = &body[..end];
        let value = lookup(kind, name, source)?;
        if looks_unresolved(&value) {
            return Err(format!(
                "{} holds an unresolved secret reference (it starts with `vault:`): the secret \
                 injector did not run for this container",
                kind.describe(name)
            ));
        }
        out.push_str(&value);
        if !value.is_empty() {
            resolved.secrets.push(value);
        }
        rest = &body[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

#[derive(Debug, Clone, Copy)]
enum Kind {
    Env,
    File,
}

impl Kind {
    fn describe(self, name: &str) -> String {
        match self {
            Kind::Env => format!("environment variable {name}"),
            Kind::File => format!("file {name}"),
        }
    }
}

fn lookup(kind: Kind, name: &str, source: &dyn RefSource) -> Result<String, String> {
    match kind {
        Kind::Env => {
            if !is_env_name(name) {
                return Err(format!(
                    "invalid environment variable name {name:?} in ${{env:…}}"
                ));
            }
            source
                .env(name)
                .ok_or_else(|| format!("references environment variable {name}, which is not set"))
        }
        Kind::File => {
            if !name.starts_with('/') {
                return Err(format!("${{file:…}} needs an absolute path, got {name:?}"));
            }
            source
                .file(name)
                .map(|s| s.trim_end_matches(['\n', '\r']).to_owned())
                .map_err(|e| format!("references file {name}, which cannot be read: {e}"))
        }
    }
}

fn is_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Whether `value` is a reference a secret injector should have replaced.
pub fn looks_unresolved(value: &str) -> bool {
    let v = value.trim_start();
    v.starts_with("vault:") || v.starts_with(">>vault:")
}

fn truncate(s: &str) -> &str {
    match s.char_indices().nth(40) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;

    struct Fake {
        env: HashMap<&'static str, &'static str>,
        files: HashMap<&'static str, &'static str>,
    }

    impl RefSource for Fake {
        fn env(&self, name: &str) -> Option<String> {
            self.env.get(name).map(|v| (*v).to_owned())
        }
        fn file(&self, path: &str) -> std::io::Result<String> {
            self.files
                .get(path)
                .map(|v| (*v).to_owned())
                .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))
        }
    }

    fn fake() -> Fake {
        Fake {
            env: HashMap::from([
                ("TOKEN", "glpat-secret"),
                ("EMPTY", ""),
                ("UNRESOLVED", "vault:secret/data/gitlab#token"),
            ]),
            files: HashMap::from([("/run/secrets/pw", "hunter2\n")]),
        }
    }

    #[test]
    fn resolves_env_and_file_references() {
        let mut v = json!({
            "token": "${env:TOKEN}",
            "nested": [{"pw": "${file:/run/secrets/pw}"}],
            "mixed": "Bearer ${env:TOKEN}!",
            "empty": "${env:EMPTY}",
            "plain": "no refs",
            "number": 3,
        });
        let resolved = resolve_refs(&mut v, &fake()).unwrap();
        assert_eq!(v["token"], "glpat-secret");
        assert_eq!(v["nested"][0]["pw"], "hunter2");
        assert_eq!(v["mixed"], "Bearer glpat-secret!");
        assert_eq!(v["empty"], "");
        assert_eq!(v["plain"], "no refs");
        assert_eq!(
            resolved.redact("bad value \"glpat-secret\""),
            "bad value \"<secret>\""
        );
    }

    #[test]
    fn redacts_escaped_forms() {
        let secret = "-----BEGIN KEY-----\nab\"c\n-----END KEY-----";
        let r = Resolved {
            secrets: vec![secret.to_owned()],
        };
        let debug = format!("bad value {secret:?}");
        let json = format!("bad value {}", Value::String(secret.to_owned()));
        for m in [debug, json, format!("raw {secret}")] {
            let out = r.redact(&m);
            assert!(!out.contains("ab"), "{out}");
            assert!(out.contains("<secret>"), "{out}");
        }
    }

    #[test]
    fn escapes_and_other_placeholders_stay() {
        let mut v = json!({
            "escaped": "$${env:TOKEN}",
            "template": "${{ repository.name }} costs $5",
            "dollar": "$",
        });
        resolve_refs(&mut v, &fake()).unwrap();
        assert_eq!(v["escaped"], "${env:TOKEN}");
        assert_eq!(v["template"], "${{ repository.name }} costs $5");
        assert_eq!(v["dollar"], "$");
    }

    #[test]
    fn errors_name_the_field_and_never_the_value() {
        let mut v = json!({
            "connections": [{"config": {
                "a": "${env:MISSING}",
                "b": "${file:/nope}",
                "c": "${file:relative}",
                "d": "${env:UNRESOLVED}",
                "e": "${env:TOKEN",
                "f": "${env:BAD-NAME}",
            }}]
        });
        let issues = resolve_refs(&mut v, &fake()).unwrap_err();
        let text: Vec<String> = issues.iter().map(ToString::to_string).collect();
        assert_eq!(issues.len(), 6, "{text:?}");
        assert!(text[0].starts_with("/connections/0/config/a: "), "{text:?}");
        assert!(text[0].contains("MISSING") && text[0].contains("not set"));
        assert!(text[1].contains("/nope"));
        assert!(text[2].contains("absolute path"));
        assert!(text[3].contains("unresolved secret reference"));
        assert!(!text[3].contains("secret/data"), "{text:?}");
        assert!(text[4].contains("unterminated"));
        assert!(text[5].contains("invalid environment variable name"));
        assert!(text.iter().all(|t| !t.contains("glpat-secret")));
    }

    #[test]
    fn detects_references() {
        assert!(has_refs(&json!({"a": ["${env:X}"]})));
        assert!(!has_refs(&json!({"a": ["$x", 1]})));
    }
}
