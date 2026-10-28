//! The Gitea connection configuration.
//!
//! The JSON schema is generated from [`GiteaConnection`] with `schemars`. Unknown keys are
//! rejected (`additionalProperties: false`).

use std::sync::LazyLock;

use regex::Regex;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sourcegull_config::{ConfigError, ConfigIssue, parse_jsonc, secrets};
use url::Url;

pub use sourcegull_config::secrets::REDACTED;

/// `repositoryQuery` entry that lists nothing beyond `orgs` and `repos`.
pub const QUERY_NONE: &str = "none";
/// `repositoryQuery` entry that lists the repositories the token's user owns, collaborates on
/// or reaches as an organization member.
pub const QUERY_AFFILIATED: &str = "affiliated";
/// `repositoryQuery` entry that lists every repository the token can read, public ones of
/// other users included (not allowed on gitea.com).
pub const QUERY_ALL: &str = "all";
/// Default `repositoryPathPattern`.
pub const DEFAULT_PATH_PATTERN: &str = "{host}/{nameWithOwner}";
/// Default `rateLimit.requestsPerHour` (10 per second). Gitea has no API rate limit of its own.
pub const DEFAULT_REQUESTS_PER_HOUR: f64 = 36_000.0;

/// A Gitea connection (`external_services.config` of kind `GITEA`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[schemars(title = "Gitea connection")]
pub struct GiteaConnection {
    /// URL of the Gitea instance, e.g. `https://gitea.example.com` (with its path when Gitea is
    /// served under one, e.g. `https://example.com/git`).
    #[schemars(regex(pattern = r"^https?://"), url)]
    pub url: String,

    /// An access token with read access to repositories (scope `read:repository`), and
    /// `read:organization` and `read:user` for `orgs` and `affiliated`.
    #[schemars(length(min = 1))]
    pub token: String,

    /// Rate limit for requests to this Gitea instance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimit>,

    /// Clone over HTTP(S) with the token (`http`) or over SSH with keys configured on the
    /// repo nodes (`ssh`).
    #[serde(rename = "gitURLType", default)]
    pub git_url_type: GitUrlType,

    /// TLS certificate (PEM) of a Gitea instance with a self-signed or internal CA.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(regex(pattern = r"^-----BEGIN CERTIFICATE-----\n"))]
    pub certificate: Option<String>,

    /// Organizations whose repositories are mirrored (every repository the token can see).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(inner(regex(pattern = r"^[\w.-]+$")))]
    pub orgs: Vec<String>,

    /// Repositories to mirror, as `owner/name`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(inner(regex(pattern = r"^[\w.-]+/[\w.-]+$")))]
    pub repos: Vec<String>,

    /// Further repositories to mirror: `affiliated` (repositories the token's user owns,
    /// collaborates on or reaches as an organization member), `all` (every repository the
    /// token can read; not on gitea.com), `none`, or a repository search keyword, matched
    /// against repository names.
    #[serde(default = "default_repository_query")]
    #[schemars(length(min = 1))]
    pub repository_query: Vec<String>,

    /// Repositories to skip, even when listed by `orgs`, `repos` or `repositoryQuery`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<Exclude>,

    /// How a repository's Sourcegull name is formed. `{host}` is the Gitea host and
    /// `{nameWithOwner}` the repository's `owner/name`.
    #[serde(default = "default_path_pattern")]
    pub repository_path_pattern: String,

    /// Regex replacements applied, in order, to repo names after `repositoryPathPattern`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub name_transformations: Vec<NameTransformation>,

    /// How Sourcegull users map to Gitea users for repository permissions. Without it, every
    /// user can see every repository from this connection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorization: Option<Authorization>,

    /// Secrets of webhooks pointed at Sourcegull (Gitea signs each delivery with the secret in
    /// `X-Gitea-Signature`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub webhooks: Vec<Webhook>,
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

/// An entry of `exclude`. A repository is excluded when any set field matches.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Exclude {
    /// `owner/name`, compared case-insensitively.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Repository id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<i64>,
    /// Regular expression matched against `owner/name`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
    /// Exclude archived repositories.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived: Option<bool>,
    /// Exclude forks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forks: Option<bool>,
    /// Exclude pull mirrors of other repositories.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mirrors: Option<bool>,
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
    /// How Sourcegull users are matched to Gitea users.
    pub identity_provider: IdentityProvider,
}

/// `authorization.identityProvider`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum IdentityProvider {
    /// Users who signed in with Gitea OAuth; their tokens are used (sign-in then also asks
    /// for the `read:repository` scope).
    Oauth,
    /// Users matched by username (the connection token must belong to a site administrator,
    /// which can act as other users with `Sudo`).
    Username,
}

/// An entry of `webhooks`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Webhook {
    /// The secret configured on the Gitea webhook.
    #[schemars(length(min = 1))]
    pub secret: String,
}

fn default_true() -> bool {
    true
}
fn default_requests_per_hour() -> f64 {
    DEFAULT_REQUESTS_PER_HOUR
}
fn default_repository_query() -> Vec<String> {
    vec![QUERY_NONE.to_owned()]
}
fn default_path_pattern() -> String {
    DEFAULT_PATH_PATTERN.to_owned()
}

impl GiteaConnection {
    /// The instance's base URL, normalized with a trailing slash and without query or
    /// fragment, e.g. `https://gitea.example.com/` (the `service_id` of its repos).
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

/// Normalizes a Gitea URL to its base: scheme, host, port and path with a trailing slash. A
/// URL of the API (`…/api/v1`) is normalized to the instance it belongs to.
pub fn normalize_base_url(raw: &str) -> Result<Url, String> {
    let mut url = Url::parse(raw.trim()).map_err(|e| format!("invalid url {raw:?}: {e}"))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(format!("url {raw:?} must be an http(s) URL with a host"));
    }
    url.set_query(None);
    url.set_fragment(None);
    let _ = url.set_username("");
    let _ = url.set_password(None);
    let path = url.path().trim_end_matches('/');
    let path = path.strip_suffix("/api/v1").unwrap_or(path);
    let path = format!("{path}/");
    url.set_path(&path);
    Ok(url)
}

/// Whether `base` (a normalized base URL) is gitea.com.
pub fn is_gitea_dot_com(base: &Url) -> bool {
    base.host_str()
        .is_some_and(|h| h.eq_ignore_ascii_case("gitea.com"))
}

/// The REST API root of an instance: `<base>api/v1/`.
pub fn api_url(base: &Url) -> Result<Url, String> {
    base.join("api/v1/").map_err(|e| e.to_string())
}

/// A validated connection with non-fatal findings.
#[derive(Debug, Clone, PartialEq)]
pub struct ValidatedConnection {
    pub config: GiteaConnection,
    pub warnings: Vec<ConfigIssue>,
}

static SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    serde_json::to_value(schemars::schema_for!(GiteaConnection)).unwrap_or(Value::Bool(true))
});

static VALIDATOR: LazyLock<Result<jsonschema::Validator, String>> =
    LazyLock::new(|| jsonschema::draft7::new(&SCHEMA).map_err(|e| e.to_string()));

/// The JSON Schema (draft 7) of a Gitea connection config, for the admin UI editor.
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
/// `nameTransformations`, a `repositoryPathPattern` without `{nameWithOwner}`, and
/// `repositoryQuery: all` on gitea.com (it would list every public repository).
pub fn validate_connection(text: &str) -> Result<ValidatedConnection, ConfigError> {
    let value = parse_jsonc(text)?;
    let validator = VALIDATOR.as_ref().map_err(|e| {
        ConfigError::Invalid(vec![issue(
            "",
            format!("internal error: Gitea schema does not compile: {e}"),
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
    let config: GiteaConnection = serde_json::from_value(value)
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
                "must set at least one of name, id, pattern, archived, forks or mirrors",
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
    if !config.repository_path_pattern.contains("{nameWithOwner}") {
        errors.push(issue(
            "/repositoryPathPattern",
            "must contain {nameWithOwner} so that repo names are unique",
        ));
    }
    for (i, q) in config.repository_query.iter().enumerate() {
        if q.trim().is_empty() {
            errors.push(issue(format!("/repositoryQuery/{i}"), "must not be empty"));
        } else if q == QUERY_ALL && base.as_ref().is_some_and(is_gitea_dot_com) {
            errors.push(issue(
                format!("/repositoryQuery/{i}"),
                "\"all\" lists every public repository of gitea.com; use orgs, repos, \
                 affiliated or a search keyword there",
            ));
        }
    }
    if config.repository_query.iter().all(|q| q == QUERY_NONE)
        && config.repos.is_empty()
        && config.orgs.is_empty()
    {
        warnings.push(issue(
            "/repositoryQuery",
            "no repositories are selected: set orgs, repos or repositoryQuery",
        ));
    }
    if !errors.is_empty() {
        return Err(ConfigError::Invalid(errors));
    }
    Ok(ValidatedConnection { config, warnings })
}

/// Replaces every secret (`token`, `webhooks[].secret`) in a JSONC configuration with
/// [`REDACTED`], keeping comments and formatting.
pub fn redact(text: &str) -> Result<String, ConfigError> {
    secrets::redact_secrets(text, secrets::token_and_webhook_secrets)
}

/// Restores secrets that an admin sent back as [`REDACTED`] from the previous configuration
/// `old` (see [`secrets::unredact_secrets`]).
pub fn unredact(new: &str, old: &str) -> Result<String, ConfigError> {
    secrets::unredact_secrets(new, old, secrets::token_and_webhook_secrets)
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
                "url": "https://gitea.example.com",
                "token": "t0k",
                "repos": ["acme/api"],
            }"#,
        )
        .unwrap();
        let c = v.config;
        assert_eq!(c.git_url_type, GitUrlType::Http);
        assert_eq!(c.repository_query, ["none"]);
        assert_eq!(c.repository_path_pattern, DEFAULT_PATH_PATTERN);
        assert_eq!(c.requests_per_hour(), Some(36_000.0));
        let base = c.base_url().unwrap();
        assert_eq!(base.as_str(), "https://gitea.example.com/");
        assert_eq!(
            api_url(&base).unwrap().as_str(),
            "https://gitea.example.com/api/v1/"
        );
        assert!(v.warnings.is_empty(), "{:?}", v.warnings);
    }

    #[test]
    fn base_urls_with_paths_and_api_urls() {
        let base = normalize_base_url("https://example.com/git?x=1").unwrap();
        assert_eq!(base.as_str(), "https://example.com/git/");
        assert_eq!(
            api_url(&base).unwrap().as_str(),
            "https://example.com/git/api/v1/"
        );
        assert_eq!(
            normalize_base_url("https://u:p@gitea.example.com/api/v1/")
                .unwrap()
                .as_str(),
            "https://gitea.example.com/"
        );
    }

    #[test]
    fn rejects_schema_and_semantic_errors() {
        let err = validate_connection(
            r#"{"url": "ftp://x", "token": "", "bogus": 1, "repos": ["no-slash"]}"#,
        )
        .unwrap_err();
        let m = messages(err).join("\n");
        assert!(m.contains("/token"), "{m}");
        assert!(m.contains("bogus"), "{m}");
        assert!(m.contains("/repos/0"), "{m}");

        let err = validate_connection(
            r#"{"url": "https://gitea.com", "token": "t",
                "repositoryQuery": ["all", "affiliated"],
                "exclude": [{"pattern": "("}, {}],
                "nameTransformations": [{"regex": "[", "replacement": ""}],
                "repositoryPathPattern": "{host}/x"}"#,
        )
        .unwrap_err();
        let m = messages(err);
        assert_eq!(m.len(), 5, "{m:?}");
        assert!(m[0].starts_with("/exclude/0/pattern"), "{m:?}");
        assert!(m[1].starts_with("/exclude/1"), "{m:?}");
        assert!(m[2].starts_with("/nameTransformations/0/regex"), "{m:?}");
        assert!(m[3].starts_with("/repositoryPathPattern"), "{m:?}");
        assert!(m[4].starts_with("/repositoryQuery/0"), "{m:?}");

        // A self-hosted instance may list every repository.
        validate_connection(
            r#"{"url": "https://git.corp", "token": "t", "repositoryQuery": ["all"]}"#,
        )
        .unwrap();
    }

    #[test]
    fn authorization_identity_providers() {
        for (ty, want) in [
            ("oauth", IdentityProvider::Oauth),
            ("username", IdentityProvider::Username),
        ] {
            let v = validate_connection(&format!(
                r#"{{"url": "https://git.corp", "token": "t", "orgs": ["o"],
                    "authorization": {{"identityProvider": {{"type": "{ty}"}}}}}}"#
            ))
            .unwrap();
            assert_eq!(v.config.authorization.unwrap().identity_provider, want);
        }
        assert!(
            validate_connection(
                r#"{"url": "https://git.corp", "token": "t", "orgs": ["o"],
                    "authorization": {"identityProvider": {"type": "saml"}}}"#
            )
            .is_err()
        );
    }

    #[test]
    fn warns_when_nothing_is_selected() {
        let v = validate_connection(r#"{"url": "https://git.corp", "token": "t"}"#).unwrap();
        let pointers: Vec<_> = v.warnings.iter().map(|w| w.pointer.as_str()).collect();
        assert_eq!(pointers, ["/repositoryQuery"]);
        let v = validate_connection(r#"{"url": "https://git.corp", "token": "t", "orgs": ["o"]}"#)
            .unwrap();
        assert_eq!(v.warnings, []);
    }

    #[test]
    fn redact_and_unredact() {
        let text = r#"{
  // the token
  "url": "https://git.corp",
  "token": "s3cret",
  "webhooks": [{"secret": "hook1"}]
}"#;
        let red = redact(text).unwrap();
        assert!(red.contains("// the token"));
        assert!(!red.contains("s3cret") && !red.contains("hook1"));
        assert_eq!(unredact(&red, text).unwrap(), text);
    }

    #[test]
    fn schema_is_draft7_with_the_expected_keys() {
        let s = connection_schema();
        let props = s["properties"].as_object().unwrap();
        for key in [
            "url",
            "token",
            "rateLimit",
            "gitURLType",
            "certificate",
            "orgs",
            "repos",
            "repositoryQuery",
            "exclude",
            "repositoryPathPattern",
            "nameTransformations",
            "authorization",
            "webhooks",
        ] {
            assert!(props.contains_key(key), "missing {key}");
        }
        assert_eq!(s["additionalProperties"], false);
        assert_eq!(s["required"], serde_json::json!(["token", "url"]));
    }
}
