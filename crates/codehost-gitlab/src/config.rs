//! The GitLab connection configuration.
//!
//! The JSON schema (`gitlab.schema.json`) is generated from [`GitLabConnection`] with
//! `schemars`. Unknown keys are rejected (`additionalProperties: false`).

use std::sync::LazyLock;

use regex::Regex;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sourcegull_config::{ConfigError, ConfigIssue, parse_jsonc, secrets};
use url::Url;

pub use sourcegull_config::secrets::REDACTED;

/// Default `projectQuery`: list nothing beyond the explicit `projects`.
pub const DEFAULT_PROJECT_QUERY: &str = "none";
/// Default `repositoryPathPattern`.
pub const DEFAULT_PATH_PATTERN: &str = "{host}/{pathWithNamespace}";
/// Default `rateLimit.requestsPerHour`.
pub const DEFAULT_REQUESTS_PER_HOUR: f64 = 36_000.0;

/// A GitLab connection (`external_services.config` of kind `GITLAB`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[schemars(title = "GitLab connection")]
pub struct GitLabConnection {
    /// URL of the GitLab instance, e.g. `https://gitlab.com` or `https://gitlab.example.com`.
    #[schemars(regex(pattern = r"^https?://"), url)]
    pub url: String,

    /// A personal access token (scope `read_api`, or `api` for bulk changes) or an OAuth
    /// token (see `token.type`).
    #[schemars(length(min = 1))]
    pub token: String,

    /// The kind of `token`.
    #[serde(rename = "token.type", default)]
    pub token_type: TokenType,

    /// OAuth refresh token (only with `"token.type": "oauth"`). The token is refreshed with
    /// the OAuth application of the `auth.providers` GitLab entry for the same URL, and the new
    /// tokens are written back here.
    #[serde(
        rename = "token.oauth.refresh",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub token_oauth_refresh: Option<String>,

    /// OAuth token expiry as a Unix timestamp in seconds (only with `"token.type": "oauth"`).
    #[serde(
        rename = "token.oauth.expiry",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub token_oauth_expiry: Option<i64>,

    /// Rate limit for requests to this GitLab instance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimit>,

    /// Clone over HTTP(S) with the token (`http`) or over SSH with keys configured on the
    /// repo nodes (`ssh`).
    #[serde(rename = "gitURLType", default)]
    pub git_url_type: GitUrlType,

    /// TLS certificate (PEM) of a self-managed instance with a self-signed or internal CA.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(regex(pattern = r"^-----BEGIN CERTIFICATE-----\n"))]
    pub certificate: Option<String>,

    /// Projects to mirror, by full path (`group/project`) or numeric id.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub projects: Vec<ProjectRef>,

    /// Projects to skip, even when listed by `projects` or `projectQuery`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<Exclude>,

    /// API paths (with query strings) that list projects to mirror, relative to `/api/v4/`,
    /// e.g. `projects?membership=true&archived=no` or `groups/mygroup/projects`. `none`
    /// lists nothing.
    #[serde(default = "default_project_query")]
    #[schemars(length(min = 1))]
    pub project_query: Vec<String>,

    /// How a project's repo name is formed. `{host}` is the GitLab host and
    /// `{pathWithNamespace}` the project's full path.
    #[serde(default = "default_path_pattern")]
    pub repository_path_pattern: String,

    /// Regex replacements applied, in order, to repo names after `repositoryPathPattern`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub name_transformations: Vec<NameTransformation>,

    /// Treat internal projects as public (visible to every Sourcegull user).
    #[serde(default)]
    pub mark_internal_repos_as_public: bool,

    /// How Sourcegull users map to GitLab users for repository permissions. Without it,
    /// every user can see every repo from this connection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorization: Option<Authorization>,

    /// Secrets of webhooks pointed at Sourcegull (sent by GitLab as `X-Gitlab-Token`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub webhooks: Vec<Webhook>,

    /// Create and update a webhook on every synced project, pointing at this connection's
    /// webhook URL (built from `externalURL`) with the first `webhooks` secret. The token needs
    /// the `api` scope and the Maintainer role on the projects.
    #[serde(rename = "webhooks.autoRegister", default)]
    pub webhooks_auto_register: bool,

    /// Accept GitLab system hook events (self-managed GitLab only): project create, destroy,
    /// rename and transfer trigger a sync of this connection.
    #[serde(default)]
    pub system_hook: bool,
}

/// `token.type`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum TokenType {
    /// Personal (or group/project) access token, sent as `PRIVATE-TOKEN`.
    #[default]
    Pat,
    /// OAuth access token, sent as `Authorization: Bearer`.
    Oauth,
}

/// `gitURLType`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum GitUrlType {
    #[default]
    Http,
    Ssh,
}

/// `rateLimit`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RateLimit {
    /// Whether requests are rate limited.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Requests allowed per hour (0 blocks all requests).
    #[serde(default = "default_requests_per_hour")]
    #[schemars(range(min = 0))]
    pub requests_per_hour: f64,
}

/// An entry of `projects`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum ProjectRef {
    /// By full path.
    Name {
        /// The project's full path, e.g. `group/subgroup/project`.
        #[schemars(regex(pattern = r"^[\w.-]+(/[\w.-]+)+$"))]
        name: String,
    },
    /// By numeric id.
    Id {
        /// The project id.
        id: i64,
    },
}

/// An entry of `exclude`. A project is excluded when any set field matches.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Exclude {
    /// Full path, compared case-insensitively.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Project id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<i64>,
    /// Regular expression matched against the full path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
    /// Exclude projects without commits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub empty_repos: Option<bool>,
}

/// An entry of `nameTransformations`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NameTransformation {
    /// Regular expression to replace.
    pub regex: String,
    /// Replacement; `$1`, `${name}` refer to capture groups.
    pub replacement: String,
}

/// `authorization`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Authorization {
    /// How Sourcegull users are matched to GitLab users.
    pub identity_provider: IdentityProvider,
}

/// `authorization.identityProvider`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum IdentityProvider {
    /// Users who signed in with GitLab OAuth; their tokens are used.
    Oauth,
    /// Users matched by username (the connection token needs admin `sudo`).
    Username,
}

/// An entry of `webhooks`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Webhook {
    /// The secret token configured on the GitLab webhook.
    #[schemars(length(min = 1))]
    pub secret: String,
}

fn default_true() -> bool {
    true
}
fn default_requests_per_hour() -> f64 {
    DEFAULT_REQUESTS_PER_HOUR
}
fn default_project_query() -> Vec<String> {
    vec![DEFAULT_PROJECT_QUERY.to_owned()]
}
fn default_path_pattern() -> String {
    DEFAULT_PATH_PATTERN.to_owned()
}

impl GitLabConnection {
    /// The instance's base URL, normalized with a trailing slash and without query or
    /// fragment, e.g. `https://gitlab.com/` (the `service_id` of its repos).
    pub fn base_url(&self) -> Result<Url, String> {
        normalize_base_url(&self.url)
    }

    /// The effective rate limit in requests per hour; `None` when disabled.
    pub fn requests_per_hour(&self) -> Option<f64> {
        match &self.rate_limit {
            Some(r) if !r.enabled => None,
            Some(r) => Some(r.requests_per_hour),
            None => Some(DEFAULT_REQUESTS_PER_HOUR),
        }
    }

    /// The configured webhook secrets.
    pub fn webhook_secrets(&self) -> impl Iterator<Item = &str> {
        self.webhooks.iter().map(|w| w.secret.as_str())
    }
}

/// Normalizes a GitLab URL to its base: scheme, host, port and path with a trailing slash.
pub fn normalize_base_url(raw: &str) -> Result<Url, String> {
    let mut url = Url::parse(raw.trim()).map_err(|e| format!("invalid url {raw:?}: {e}"))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(format!("url {raw:?} must be an http(s) URL with a host"));
    }
    url.set_query(None);
    url.set_fragment(None);
    let _ = url.set_username("");
    let _ = url.set_password(None);
    if !url.path().ends_with('/') {
        let path = format!("{}/", url.path());
        url.set_path(&path);
    }
    Ok(url)
}

/// A validated connection with non-fatal findings.
#[derive(Debug, Clone, PartialEq)]
pub struct ValidatedConnection {
    pub config: GitLabConnection,
    pub warnings: Vec<ConfigIssue>,
}

static SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    serde_json::to_value(schemars::schema_for!(GitLabConnection)).unwrap_or(Value::Bool(true))
});

static VALIDATOR: LazyLock<Result<jsonschema::Validator, String>> =
    LazyLock::new(|| jsonschema::draft7::new(&SCHEMA).map_err(|e| e.to_string()));

/// The JSON Schema (draft 7) of a GitLab connection config, for the admin UI editor.
pub fn connection_schema() -> Value {
    SCHEMA.clone()
}

fn issue(pointer: impl Into<String>, message: impl Into<String>) -> ConfigIssue {
    ConfigIssue {
        pointer: pointer.into(),
        message: message.into(),
    }
}

/// Parses JSONC `text`, validates it against the schema and the semantic rules, and
/// deserializes it.
///
/// Errors: schema violations, an unparsable `url`, invalid regexes in `exclude` or
/// `nameTransformations`, a `repositoryPathPattern` without `{pathWithNamespace}`, and a
/// gitlab.com `projectQuery` that would list every public project. The OAuth fields only make
/// sense with `"token.type": "oauth"` and produce warnings otherwise.
pub fn validate_connection(text: &str) -> Result<ValidatedConnection, ConfigError> {
    let value = parse_jsonc(text)?;
    let validator = VALIDATOR.as_ref().map_err(|e| {
        ConfigError::Invalid(vec![issue(
            "",
            format!("internal error: GitLab schema does not compile: {e}"),
        )])
    })?;
    let mut issues: Vec<ConfigIssue> = validator
        .iter_errors(&value)
        .map(|e| issue(e.instance_path.to_string(), e.to_string()))
        .collect();
    if !issues.is_empty() {
        issues.sort_by(|a, b| a.pointer.cmp(&b.pointer));
        return Err(ConfigError::Invalid(issues));
    }
    let config: GitLabConnection = serde_json::from_value(value)
        .map_err(|e| ConfigError::Invalid(vec![issue("", e.to_string())]))?;

    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    let base = match config.base_url() {
        Ok(u) => Some(u),
        Err(e) => {
            errors.push(issue("/url", e));
            None
        }
    };
    for (i, ex) in config.exclude.iter().enumerate() {
        if *ex == Exclude::default() {
            errors.push(issue(
                format!("/exclude/{i}"),
                "must set at least one of name, id, pattern or emptyRepos",
            ));
        }
        if let Some(p) = &ex.pattern
            && let Err(e) = Regex::new(p)
        {
            errors.push(issue(
                format!("/exclude/{i}/pattern"),
                format!("invalid regex: {e}"),
            ));
        }
    }
    for (i, t) in config.name_transformations.iter().enumerate() {
        if let Err(e) = Regex::new(&t.regex) {
            errors.push(issue(
                format!("/nameTransformations/{i}/regex"),
                format!("invalid regex: {e}"),
            ));
        }
    }
    if !config
        .repository_path_pattern
        .contains("{pathWithNamespace}")
    {
        errors.push(issue(
            "/repositoryPathPattern",
            "must contain {pathWithNamespace} so that repo names are unique",
        ));
    }
    for (i, q) in config.project_query.iter().enumerate() {
        if q == DEFAULT_PROJECT_QUERY {
            continue;
        }
        if q.starts_with('/') || q.contains("://") {
            errors.push(issue(
                format!("/projectQuery/{i}"),
                "must be an API path relative to /api/v4/, e.g. projects?membership=true",
            ));
            continue;
        }
        if base.as_ref().and_then(Url::host_str) == Some("gitlab.com") && lists_all_projects(q) {
            errors.push(issue(
                format!("/projectQuery/{i}"),
                "on gitlab.com, a projects query must be scoped with membership=true or \
                 owned=true (or use groups/<group>/projects); otherwise it lists every public \
                 project",
            ));
        }
    }
    if config.token_type == TokenType::Pat
        && (config.token_oauth_refresh.is_some() || config.token_oauth_expiry.is_some())
    {
        warnings.push(issue(
            "/token.oauth.refresh",
            "token.oauth.* is ignored unless token.type is \"oauth\"",
        ));
    }
    if config.webhooks_auto_register && config.webhook_secrets().all(str::is_empty) {
        warnings.push(issue(
            "/webhooks.autoRegister",
            "webhooks.autoRegister needs a webhooks secret; generate one on the connection page",
        ));
    }
    if config
        .project_query
        .iter()
        .all(|q| q == DEFAULT_PROJECT_QUERY)
        && config.projects.is_empty()
    {
        warnings.push(issue(
            "/projectQuery",
            "no projects are selected: set projects or projectQuery",
        ));
    }
    if !errors.is_empty() {
        return Err(ConfigError::Invalid(errors));
    }
    Ok(ValidatedConnection { config, warnings })
}

/// Whether a `projectQuery` entry lists all projects of the instance (an unscoped `projects`
/// query).
fn lists_all_projects(query: &str) -> bool {
    let (path, qs) = query.split_once('?').unwrap_or((query, ""));
    if path.trim_end_matches('/') != "projects" {
        return false;
    }
    !url::form_urlencoded::parse(qs.as_bytes()).any(|(k, v)| {
        matches!(k.as_ref(), "membership" | "owned" | "starred")
            && matches!(v.as_ref(), "true" | "1" | "yes")
    })
}

/// JSON pointers of the secret fields in a parsed configuration.
fn secret_pointers(value: &Value) -> Vec<String> {
    let mut out: Vec<String> = ["token", "token.oauth.refresh"]
        .into_iter()
        .filter(|key| value.get(key).is_some_and(Value::is_string))
        .map(|key| format!("/{key}"))
        .collect();
    out.extend(secrets::webhook_secret_pointers(value));
    out
}

/// Replaces every secret in a JSONC configuration with [`REDACTED`], keeping comments and
/// formatting. Fails only if `text` is not valid JSONC.
pub fn redact(text: &str) -> Result<String, ConfigError> {
    secrets::redact_secrets(text, secret_pointers)
}

/// Restores secrets that an admin sent back as [`REDACTED`] from the previous configuration
/// `old`. Returns `new` verbatim when it contains no placeholder; otherwise `new` with the
/// placeholders replaced in place (comments and formatting kept), or the merged configuration
/// as plain JSON when the placeholders cannot be matched to their fields in the text. Fails
/// when a placeholder has no counterpart in `old`.
pub fn unredact(new: &str, old: &str) -> Result<String, ConfigError> {
    let Some(secrets::Restored {
        mut value,
        mut text,
        pointers,
    }) = secrets::restore_secrets(new, old, secret_pointers)?
    else {
        return Ok(new.to_owned());
    };
    let old = parse_jsonc(old)?;
    // A kept token keeps its stored expiry: the expiry in `new` is not redacted and may
    // belong to a token that has been refreshed since `new` was read.
    if pointers.iter().any(|p| p == "/token")
        && let Some(obj) = value.as_object()
        && obj.get("token.oauth.expiry") != old.get("token.oauth.expiry")
    {
        let before = value.clone();
        let obj = value.as_object_mut().expect("checked above");
        match old.get("token.oauth.expiry") {
            Some(expiry) => obj.insert("token.oauth.expiry".to_owned(), expiry.clone()),
            None => obj.remove("token.oauth.expiry"),
        };
        text = text.and_then(|t| splice_tokens(&t, &before, &value));
    }
    match text {
        Some(text) => Ok(text),
        None => secrets::to_pretty(&value),
    }
}

/// The first webhook secret of a configuration (the one to configure on GitLab), if any.
pub fn webhook_secret(text: &str) -> Option<String> {
    secrets::webhook_secret(text)
}

/// Sets the configuration's webhook secrets to just `secret`. When the configuration has
/// exactly one secret it is replaced in the text (comments and formatting kept); otherwise
/// the configuration is rewritten as plain JSON.
pub fn set_webhook_secret(text: &str, secret: &str) -> Result<String, ConfigError> {
    secrets::set_webhook_secret(text, secret)
}

/// The configuration with its OAuth tokens (`token`, `token.oauth.refresh`,
/// `token.oauth.expiry`) replaced and every other setting unchanged. The values are replaced in
/// the text when each old one appears there once (comments and formatting kept); otherwise the
/// configuration is rewritten as plain JSON.
pub fn set_oauth_tokens(
    text: &str,
    tokens: &crate::token::OAuthTokens,
) -> Result<String, ConfigError> {
    let old = parse_jsonc(text)?;
    let mut new = old.clone();
    let Some(obj) = new.as_object_mut() else {
        return Err(ConfigError::Invalid(vec![issue(
            "",
            "the configuration must be a JSON object",
        )]));
    };
    obj.insert(
        "token".to_owned(),
        Value::String(tokens.access_token.clone()),
    );
    match &tokens.refresh_token {
        Some(r) => obj.insert("token.oauth.refresh".to_owned(), Value::String(r.clone())),
        None => obj.remove("token.oauth.refresh"),
    };
    match tokens.expires_at {
        Some(at) => obj.insert("token.oauth.expiry".to_owned(), Value::from(at)),
        None => obj.remove("token.oauth.expiry"),
    };
    if let Some(spliced) = splice_tokens(text, &old, &new) {
        return Ok(spliced);
    }
    serde_json::to_string_pretty(&new)
        .map_err(|e| ConfigError::Invalid(vec![issue("", e.to_string())]))
}

/// `text` with the token values of `old` replaced by those of `new`, if every replaced value
/// appears exactly once and the result parses to `new`.
fn splice_tokens(text: &str, old: &Value, new: &Value) -> Option<String> {
    let mut out = text.to_owned();
    for key in ["token", "token.oauth.refresh", "token.oauth.expiry"] {
        let (before, after) = (old.get(key), new.get(key));
        if before == after {
            continue;
        }
        let (Some(before), Some(after)) = (before, after) else {
            return None;
        };
        let literal = before.to_string();
        let replacement = after.to_string();
        if before.is_string() {
            if out.matches(&literal).count() != 1 {
                return None;
            }
            out = out.replacen(&literal, &replacement, 1);
        } else {
            let pattern = Regex::new(&format!(
                r#""{}"\s*:\s*{}"#,
                regex::escape(key),
                regex::escape(&literal)
            ))
            .ok()?;
            let found = pattern.find(&out)?;
            if pattern.find_iter(&out).count() != 1 {
                return None;
            }
            let matched = found.as_str();
            let value_at = matched.len() - literal.len();
            let range = found.start() + value_at..found.end();
            out.replace_range(range, &replacement);
        }
    }
    (parse_jsonc(&out).ok()? == *new).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn messages(e: ConfigError) -> Vec<String> {
        match e {
            ConfigError::Invalid(issues) => issues.iter().map(ToString::to_string).collect(),
            ConfigError::Syntax(s) => vec![s.to_string()],
        }
    }

    #[test]
    fn minimal_config_gets_defaults() {
        let v = validate_connection(
            r#"{
                // comment
                "url": "https://gitlab.example.com/sub",
                "token": "glpat-x",
                "projects": [{"name": "group/project"}, {"id": 42}],
            }"#,
        )
        .unwrap();
        let c = v.config;
        assert_eq!(c.token_type, TokenType::Pat);
        assert_eq!(c.git_url_type, GitUrlType::Http);
        assert_eq!(c.project_query, ["none"]);
        assert_eq!(c.repository_path_pattern, DEFAULT_PATH_PATTERN);
        assert_eq!(c.requests_per_hour(), Some(36_000.0));
        assert_eq!(
            c.projects,
            [
                ProjectRef::Name {
                    name: "group/project".into()
                },
                ProjectRef::Id { id: 42 }
            ]
        );
        assert_eq!(
            c.base_url().unwrap().as_str(),
            "https://gitlab.example.com/sub/"
        );
        assert!(v.warnings.is_empty(), "{:?}", v.warnings);
    }

    #[test]
    fn rejects_schema_and_semantic_errors() {
        let err =
            validate_connection(r#"{"url": "ftp://x", "token": "", "bogus": 1}"#).unwrap_err();
        let m = messages(err).join("\n");
        assert!(m.contains("/token"), "{m}");
        assert!(m.contains("bogus"), "{m}");

        let err = validate_connection(
            r#"{"url": "https://gitlab.com", "token": "t",
                "projectQuery": ["projects?archived=no", "groups/g/projects", "projects?membership=true"],
                "exclude": [{"pattern": "("}],
                "nameTransformations": [{"regex": "[", "replacement": ""}],
                "repositoryPathPattern": "{host}/x"}"#,
        )
        .unwrap_err();
        let m = messages(err);
        assert_eq!(m.len(), 4, "{m:?}");
        assert!(m[0].starts_with("/exclude/0/pattern"));
        assert!(m[1].starts_with("/nameTransformations/0/regex"));
        assert!(m[2].starts_with("/repositoryPathPattern"));
        assert!(m[3].starts_with("/projectQuery/0"));

        let err = validate_connection(r#"{"url": "https://x", "token": "t", "exclude": [{}]}"#)
            .unwrap_err();
        assert!(messages(err)[0].starts_with("/exclude/0"));
        // Self-managed instances may list all projects.
        validate_connection(
            r#"{"url": "https://git.corp", "token": "t", "projectQuery": ["projects"]}"#,
        )
        .unwrap();
    }

    #[test]
    fn warnings() {
        let v = validate_connection(
            r#"{"url": "https://x", "token": "t", "token.oauth.refresh": "r",
                "webhooks.autoRegister": true}"#,
        )
        .unwrap();
        let pointers: Vec<_> = v.warnings.iter().map(|w| w.pointer.as_str()).collect();
        assert_eq!(
            pointers,
            [
                "/token.oauth.refresh",
                "/webhooks.autoRegister",
                "/projectQuery"
            ]
        );
        let v = validate_connection(
            r#"{"url": "https://x", "token": "t", "projects": [{"id": 1}],
                "webhooks": [{"secret": "s"}], "webhooks.autoRegister": true}"#,
        )
        .unwrap();
        assert_eq!(v.warnings, []);
    }

    #[test]
    fn redact_and_unredact() {
        let text = r#"{
  // the token
  "url": "https://x",
  "token": "s3cret",
  "webhooks": [{"secret": "hook1"}, {"secret": "hook2"}]
}"#;
        let red = redact(text).unwrap();
        assert!(red.contains("// the token"));
        assert!(!red.contains("s3cret") && !red.contains("hook1") && !red.contains("hook2"));
        assert_eq!(red.matches(REDACTED).count(), 3);

        // Sending the redacted text back restores every secret.
        let restored = unredact(&red, text).unwrap();
        assert!(
            restored.contains("// the token"),
            "comments kept: {restored}"
        );
        let v = parse_jsonc(&restored).unwrap();
        assert_eq!(v["token"], "s3cret");
        assert_eq!(v["webhooks"][1]["secret"], "hook2");

        // A changed secret is kept; text without placeholders is returned verbatim.
        let changed = red.replace(r#""token": "REDACTED""#, r#""token": "new""#);
        let v = parse_jsonc(&unredact(&changed, text).unwrap()).unwrap();
        assert_eq!(v["token"], "new");
        assert_eq!(unredact(text, "{}").unwrap(), text);

        // A placeholder for a secret that never existed is an error.
        let added = r#"{"url": "https://x", "token": "t", "webhooks": [{"secret": "REDACTED"}]}"#;
        assert!(unredact(added, text.replace("hook1", "REDACTED").as_str()).is_err());
    }

    #[test]
    fn unredact_keeps_stored_expiry_with_kept_token() {
        let stored = r#"{
  // oauth
  "url": "https://x", "token.type": "oauth",
  "token": "a2", "token.oauth.refresh": "r2", "token.oauth.expiry": 2000
}"#;
        let stale = r#"{
  // oauth, edited
  "url": "https://x", "token.type": "oauth",
  "token": "REDACTED", "token.oauth.refresh": "REDACTED", "token.oauth.expiry": 1000
}"#;
        let merged = unredact(stale, stored).unwrap();
        assert!(merged.contains("// oauth, edited"), "{merged}");
        let v = parse_jsonc(&merged).unwrap();
        assert_eq!(v["token"], "a2");
        assert_eq!(v["token.oauth.refresh"], "r2");
        assert_eq!(v["token.oauth.expiry"], 2000);

        // A token entered by the admin keeps the expiry entered with it.
        let entered = stale.replace(r#""token": "REDACTED""#, r#""token": "a3""#);
        let v = parse_jsonc(&unredact(&entered, stored).unwrap()).unwrap();
        assert_eq!(v["token"], "a3");
        assert_eq!(v["token.oauth.expiry"], 1000);
    }

    #[test]
    fn unredact_keeps_comments() {
        let text = r#"{
  // the token
  "url": "https://x",
  "token": "s3cret", /* "REDACTED" in a comment stays */
  "webhooks": [{"secret": "hook1"}]
}"#;
        let red = redact(text).unwrap();
        let restored = unredact(&red, text).unwrap();
        assert_eq!(restored, text);

        // Reordered fields still get their own secrets back.
        let reordered = r#"{"webhooks": [{"secret": "REDACTED"}], // hooks
  "token": "REDACTED", "url": "https://x"}"#;
        let restored = unredact(reordered, text).unwrap();
        assert!(restored.contains("// hooks"), "{restored}");
        let v = parse_jsonc(&restored).unwrap();
        assert_eq!(v["token"], "s3cret");
        assert_eq!(v["webhooks"][0]["secret"], "hook1");
    }

    #[test]
    fn oauth_tokens_are_replaced() {
        use crate::token::OAuthTokens;
        let text = "{\n  // OAuth\n  \"url\": \"https://x\", \"token\": \"a1\",\n  \"token.type\": \"oauth\",\n  \"token.oauth.refresh\": \"r1\",\n  \"token.oauth.expiry\": 100,\n  \"projects\": [{\"name\": \"g/p\"}]\n}";
        let tokens = OAuthTokens {
            access_token: "a2".into(),
            refresh_token: Some("r2".into()),
            expires_at: Some(7300),
        };
        let out = set_oauth_tokens(text, &tokens).unwrap();
        assert!(out.contains("// OAuth"), "{out}");
        let c = validate_connection(&out).unwrap().config;
        assert_eq!(c.token, "a2");
        assert_eq!(c.token_oauth_refresh.as_deref(), Some("r2"));
        assert_eq!(c.token_oauth_expiry, Some(7300));
        assert_eq!(c.projects.len(), 1);

        // A missing expiry is added (the configuration is rewritten).
        let bare = r#"{"url": "https://x", "token": "a1", "token.type": "oauth"}"#;
        let c = validate_connection(&set_oauth_tokens(bare, &tokens).unwrap())
            .unwrap()
            .config;
        assert_eq!(c.token_oauth_expiry, Some(7300));
        assert_eq!(c.token_oauth_refresh.as_deref(), Some("r2"));
    }

    #[test]
    fn webhook_secrets_are_read_and_replaced() {
        let one = "{\n  // hooks\n  \"url\": \"https://x\", \"token\": \"t\",\n  \"webhooks\": [{\"secret\": \"old\"}]\n}";
        assert_eq!(webhook_secret(one).as_deref(), Some("old"));
        let set = set_webhook_secret(one, "new").unwrap();
        assert!(set.contains("// hooks"), "comments kept: {set}");
        assert_eq!(webhook_secret(&set).as_deref(), Some("new"));

        let none = r#"{"url": "https://x", "token": "t"}"#;
        assert_eq!(webhook_secret(none), None);
        let set = set_webhook_secret(none, "fresh").unwrap();
        assert_eq!(webhook_secret(&set).as_deref(), Some("fresh"));
        assert!(validate_connection(&set).is_ok());

        let two =
            r#"{"url": "https://x", "token": "t", "webhooks": [{"secret": "a"}, {"secret": "b"}]}"#;
        let set = set_webhook_secret(two, "c").unwrap();
        let v = parse_jsonc(&set).unwrap();
        assert_eq!(v["webhooks"], serde_json::json!([{"secret": "c"}]));
    }

    #[test]
    fn schema_is_draft7_with_the_expected_keys() {
        let s = connection_schema();
        let props = s["properties"].as_object().unwrap();
        for key in [
            "url",
            "token",
            "token.type",
            "token.oauth.refresh",
            "token.oauth.expiry",
            "rateLimit",
            "gitURLType",
            "certificate",
            "projects",
            "exclude",
            "projectQuery",
            "repositoryPathPattern",
            "nameTransformations",
            "markInternalReposAsPublic",
            "authorization",
            "webhooks",
        ] {
            assert!(props.contains_key(key), "missing {key}");
        }
        assert_eq!(s["additionalProperties"], false);
        assert_eq!(s["required"], serde_json::json!(["token", "url"]));
    }
}
