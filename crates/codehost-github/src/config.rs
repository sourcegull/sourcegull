//! The GitHub connection configuration.
//!
//! The JSON schema is generated from [`GitHubConnection`] with `schemars`. Unknown keys are
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
/// or can access as an organization member.
pub const QUERY_AFFILIATED: &str = "affiliated";
/// `repositoryQuery` entry that lists every public repository of the instance (GitHub
/// Enterprise Server only).
pub const QUERY_PUBLIC: &str = "public";
/// Default `repositoryPathPattern`.
pub const DEFAULT_PATH_PATTERN: &str = "{host}/{nameWithOwner}";
/// Default `rateLimit.requestsPerHour`: GitHub's limit for a personal access token.
pub const DEFAULT_REQUESTS_PER_HOUR: f64 = 5_000.0;

/// A GitHub connection (`external_services.config` of kind `GITHUB`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[schemars(title = "GitHub connection")]
pub struct GitHubConnection {
    /// URL of the GitHub instance: `https://github.com` or a GitHub Enterprise Server URL,
    /// e.g. `https://github.example.com`.
    #[schemars(regex(pattern = r"^https?://"), url)]
    pub url: String,

    /// A personal access token: fine-grained (Contents and Metadata read access to the
    /// repositories) or classic (scope `repo`, or none for public repositories only). Set
    /// either `token` or `githubApp`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1))]
    pub token: Option<String>,

    /// Authenticate as a GitHub App installation instead of with `token`: API requests and
    /// clones use installation tokens minted with the App's private key.
    #[serde(rename = "githubApp", default, skip_serializing_if = "Option::is_none")]
    pub github_app: Option<GitHubApp>,

    /// Rate limit for requests to this GitHub instance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimit>,

    /// Clone over HTTP(S) with the token (`http`) or over SSH with keys configured on the
    /// repo nodes (`ssh`).
    #[serde(rename = "gitURLType", default)]
    pub git_url_type: GitUrlType,

    /// TLS certificate (PEM) of a GitHub Enterprise Server with a self-signed or internal CA.
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
    /// collaborates on or reaches as an organization member), `public` (every public
    /// repository; GitHub Enterprise Server only), `none`, or a repository search query such
    /// as `org:my-org topic:backend` (GitHub returns at most 1000 results per query).
    #[serde(default = "default_repository_query")]
    #[schemars(length(min = 1))]
    pub repository_query: Vec<String>,

    /// Repositories to skip, even when listed by `orgs`, `repos` or `repositoryQuery`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<Exclude>,

    /// How a repository's Sourcegull name is formed. `{host}` is the GitHub host and
    /// `{nameWithOwner}` the repository's `owner/name`.
    #[serde(default = "default_path_pattern")]
    pub repository_path_pattern: String,

    /// Regex replacements applied, in order, to repo names after `repositoryPathPattern`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub name_transformations: Vec<NameTransformation>,

    /// Treat internal repositories (GitHub Enterprise) as public (visible to every Sourcegull
    /// user).
    #[serde(default)]
    pub mark_internal_repos_as_public: bool,

    /// Repository permissions from GitHub. Without it, every user can see every repository
    /// from this connection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorization: Option<Authorization>,

    /// Secrets of webhooks pointed at Sourcegull (GitHub signs each delivery with the secret in
    /// `X-Hub-Signature-256`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub webhooks: Vec<Webhook>,
}

/// `githubApp`.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GitHubApp {
    /// The App ID (on the App's settings page).
    #[serde(rename = "appID")]
    #[schemars(range(min = 1))]
    pub app_id: u64,
    /// The App's private key (PEM, `-----BEGIN RSA PRIVATE KEY-----`). Use a
    /// `${file:/path}` reference in configuration files.
    #[serde(rename = "privateKey")]
    #[schemars(length(min = 1))]
    pub private_key: String,
    /// The ID of the App's installation on the organization or account (the number at the
    /// end of the installation's settings URL).
    #[serde(rename = "installationID")]
    #[schemars(range(min = 1))]
    pub installation_id: u64,
}

impl std::fmt::Debug for GitHubApp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitHubApp")
            .field("app_id", &self.app_id)
            .field("installation_id", &self.installation_id)
            .finish_non_exhaustive()
    }
}

/// `authorization`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Authorization {
    /// How Sourcegull users are matched to GitHub users: `oauth` (users who signed in with
    /// GitHub; their tokens are used). `username` is rejected for GitHub.
    pub identity_provider: IdentityProvider,
}

/// `authorization.identityProvider`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum IdentityProvider {
    /// Users who signed in with GitHub OAuth; their tokens are used.
    Oauth,
    /// Not supported for GitHub (rejected at validation).
    Username,
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

/// An entry of `webhooks`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Webhook {
    /// The secret configured on the GitHub webhook.
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

impl GitHubConnection {
    /// The instance's base URL, normalized with a trailing slash and without query or
    /// fragment, e.g. `https://github.com/` (the `service_id` of its repos).
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

    /// The secret that identifies the connection's credentials: the token, or the GitHub App's
    /// private key.
    pub fn credential_key(&self) -> &str {
        match (&self.token, &self.github_app) {
            (Some(t), _) => t,
            (None, Some(app)) => &app.private_key,
            (None, None) => "",
        }
    }

    /// The configured webhook secrets.
    pub fn webhook_secrets(&self) -> impl Iterator<Item = &str> {
        self.webhooks.iter().map(|w| w.secret.as_str())
    }
}

/// Normalizes a GitHub URL to its base: scheme, host, port and path with a trailing slash.
/// `api.github.com` is normalized to `github.com`.
pub fn normalize_base_url(raw: &str) -> Result<Url, String> {
    let mut url = Url::parse(raw.trim()).map_err(|e| format!("invalid url {raw:?}: {e}"))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(format!("url {raw:?} must be an http(s) URL with a host"));
    }
    if url.host_str() == Some("api.github.com") {
        url = Url::parse("https://github.com/").expect("valid URL");
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

/// Whether `base` (a normalized base URL) is github.com.
pub fn is_github_dot_com(base: &Url) -> bool {
    base.host_str()
        .is_some_and(|h| h.eq_ignore_ascii_case("github.com"))
}

/// The REST API root of an instance: `https://api.github.com/` for github.com, `<base>api/v3/`
/// for GitHub Enterprise Server.
pub fn api_url(base: &Url) -> Result<Url, String> {
    if is_github_dot_com(base) {
        return Ok(Url::parse("https://api.github.com/").expect("valid URL"));
    }
    base.join("api/v3/").map_err(|e| e.to_string())
}

/// A validated connection with non-fatal findings.
#[derive(Debug, Clone, PartialEq)]
pub struct ValidatedConnection {
    pub config: GitHubConnection,
    pub warnings: Vec<ConfigIssue>,
}

static SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    serde_json::to_value(schemars::schema_for!(GitHubConnection)).unwrap_or(Value::Bool(true))
});

static VALIDATOR: LazyLock<Result<jsonschema::Validator, String>> =
    LazyLock::new(|| jsonschema::draft7::new(&SCHEMA).map_err(|e| e.to_string()));

/// The JSON Schema (draft 7) of a GitHub connection config, for the admin UI editor.
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
/// `repositoryQuery: public` on github.com (it would list every public repository), the
/// `username` identity provider (GitHub cannot read as another user), and neither or both of
/// `token` and `githubApp`, or a `githubApp.privateKey` that is not an RSA key in PEM.
pub fn validate_connection(text: &str) -> Result<ValidatedConnection, ConfigError> {
    let value = parse_jsonc(text)?;
    let validator = VALIDATOR.as_ref().map_err(|e| {
        ConfigError::Invalid(vec![issue(
            "",
            format!("internal error: GitHub schema does not compile: {e}"),
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
    let config: GitHubConnection = serde_json::from_value(value)
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
                "must set at least one of name, id, pattern, archived or forks",
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
        } else if q == QUERY_PUBLIC && base.as_ref().is_some_and(is_github_dot_com) {
            errors.push(issue(
                format!("/repositoryQuery/{i}"),
                "\"public\" lists every public repository and is only allowed on GitHub \
                 Enterprise Server; use orgs, repos or a search query on github.com",
            ));
        }
    }
    match (&config.token, &config.github_app) {
        (Some(_), Some(_)) => errors.push(issue(
            "/githubApp",
            "set either token or githubApp, not both",
        )),
        (None, None) => errors.push(issue("/token", "set either token or githubApp")),
        (None, Some(app)) => {
            if let Err(e) = crate::app::encoding_key(&app.private_key) {
                errors.push(issue("/githubApp/privateKey", e.to_string()));
            }
        }
        (Some(_), None) => {}
    }
    if config
        .authorization
        .as_ref()
        .is_some_and(|a| a.identity_provider == IdentityProvider::Username)
    {
        errors.push(issue(
            "/authorization/identityProvider/type",
            "\"username\" is not supported for GitHub: GitHub cannot list another user's \
             repositories with the connection token; use \"oauth\" (users sign in with GitHub)",
        ));
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

/// The pointers of the secrets of a configuration: `token`, `githubApp.privateKey` and
/// `webhooks[].secret`.
fn secret_pointers(value: &Value) -> Vec<String> {
    let mut out = secrets::token_and_webhook_secrets(value);
    if value
        .pointer("/githubApp/privateKey")
        .is_some_and(Value::is_string)
    {
        out.push("/githubApp/privateKey".to_owned());
    }
    out
}

/// Replaces every secret (`token`, `githubApp.privateKey`, `webhooks[].secret`) in a JSONC
/// configuration with [`REDACTED`], keeping comments and formatting.
pub fn redact(text: &str) -> Result<String, ConfigError> {
    secrets::redact_secrets(text, secret_pointers)
}

/// Restores secrets that an admin sent back as [`REDACTED`] from the previous configuration
/// `old` (see [`secrets::unredact_secrets`]).
pub fn unredact(new: &str, old: &str) -> Result<String, ConfigError> {
    secrets::unredact_secrets(new, old, secret_pointers)
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
                "url": "https://github.com",
                "token": "github_pat_x",
                "repos": ["octo/hello"],
            }"#,
        )
        .unwrap();
        let c = v.config;
        assert_eq!(c.git_url_type, GitUrlType::Http);
        assert_eq!(c.repository_query, ["none"]);
        assert_eq!(c.repository_path_pattern, DEFAULT_PATH_PATTERN);
        assert_eq!(c.requests_per_hour(), Some(5_000.0));
        assert_eq!(c.repos, ["octo/hello"]);
        let base = c.base_url().unwrap();
        assert_eq!(base.as_str(), "https://github.com/");
        assert_eq!(api_url(&base).unwrap().as_str(), "https://api.github.com/");
        assert!(v.warnings.is_empty(), "{:?}", v.warnings);
    }

    #[test]
    fn enterprise_urls() {
        let base = normalize_base_url("https://ghe.example.com/?x=1").unwrap();
        assert_eq!(base.as_str(), "https://ghe.example.com/");
        assert_eq!(
            api_url(&base).unwrap().as_str(),
            "https://ghe.example.com/api/v3/"
        );
        assert_eq!(
            normalize_base_url("https://api.github.com")
                .unwrap()
                .as_str(),
            "https://github.com/"
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
            r#"{"url": "https://github.com", "token": "t",
                "repositoryQuery": ["public", "affiliated"],
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

        let err = validate_connection(
            r#"{"url": "https://github.com", "token": "t", "orgs": ["o"],
                "authorization": {"identityProvider": {"type": "username"}}}"#,
        )
        .unwrap_err();
        let m = messages(err);
        assert_eq!(m.len(), 1, "{m:?}");
        assert!(
            m[0].starts_with("/authorization/identityProvider/type") && m[0].contains("oauth"),
            "{m:?}"
        );
        let v = validate_connection(
            r#"{"url": "https://github.com", "token": "t", "orgs": ["o"],
                "authorization": {"identityProvider": {"type": "oauth"}}}"#,
        )
        .unwrap();
        assert_eq!(
            v.config.authorization.unwrap().identity_provider,
            IdentityProvider::Oauth
        );

        // GitHub Enterprise Server may list every public repository.
        validate_connection(
            r#"{"url": "https://ghe.corp", "token": "t", "repositoryQuery": ["public"]}"#,
        )
        .unwrap();
    }

    #[test]
    fn warns_when_nothing_is_selected() {
        let v = validate_connection(r#"{"url": "https://github.com", "token": "t"}"#).unwrap();
        let pointers: Vec<_> = v.warnings.iter().map(|w| w.pointer.as_str()).collect();
        assert_eq!(pointers, ["/repositoryQuery"]);
        let v =
            validate_connection(r#"{"url": "https://github.com", "token": "t", "orgs": ["o"]}"#)
                .unwrap();
        assert_eq!(v.warnings, []);
    }

    #[test]
    fn redact_and_unredact() {
        let text = r#"{
  // the token
  "url": "https://github.com",
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
            "markInternalReposAsPublic",
            "authorization",
            "webhooks",
            "githubApp",
        ] {
            assert!(props.contains_key(key), "missing {key}");
        }
        assert_eq!(s["additionalProperties"], false);
        assert_eq!(s["required"], serde_json::json!(["url"]));
    }

    const APP_KEY: &str = include_str!("testdata/app-key.pem");

    fn app_config(key: &str, extra: &str) -> String {
        format!(
            r#"{{"url": "https://github.com", "orgs": ["o"],
                "githubApp": {{"appID": 1, "privateKey": {}, "installationID": 2}} {extra}}}"#,
            serde_json::to_string(key).unwrap()
        )
    }

    #[test]
    fn github_app_instead_of_token() {
        let c = validate_connection(&app_config(APP_KEY, ""))
            .unwrap()
            .config;
        let app = c.github_app.as_ref().unwrap();
        assert_eq!((app.app_id, app.installation_id), (1, 2));
        assert_eq!(c.token, None);
        assert_eq!(c.credential_key(), APP_KEY);
        assert!(!format!("{app:?}").contains("PRIVATE KEY"));

        // Exactly one of token and githubApp.
        let m =
            messages(validate_connection(&app_config(APP_KEY, r#", "token": "t""#)).unwrap_err());
        assert_eq!(m.len(), 1, "{m:?}");
        assert!(
            m[0].starts_with("/githubApp") && m[0].contains("not both"),
            "{m:?}"
        );
        let m = messages(
            validate_connection(r#"{"url": "https://github.com", "orgs": ["o"]}"#).unwrap_err(),
        );
        assert_eq!(m.len(), 1, "{m:?}");
        assert!(
            m[0].starts_with("/token") && m[0].contains("githubApp"),
            "{m:?}"
        );

        // A key that does not parse is rejected without quoting it.
        let m = messages(validate_connection(&app_config("s3cret-not-a-key", "")).unwrap_err());
        assert_eq!(m.len(), 1, "{m:?}");
        assert!(m[0].starts_with("/githubApp/privateKey"), "{m:?}");
        assert!(!m[0].contains("s3cret"), "{m:?}");
        // Missing fields are schema errors.
        let m = messages(
            validate_connection(
                r#"{"url": "https://github.com", "githubApp": {"appID": 1, "privateKey": "k"}}"#,
            )
            .unwrap_err(),
        );
        assert!(m.iter().any(|m| m.starts_with("/githubApp")), "{m:?}");
    }

    #[test]
    fn github_app_private_key_is_redacted() {
        let text = app_config(APP_KEY, "");
        let red = redact(&text).unwrap();
        assert!(!red.contains("PRIVATE KEY"), "{red}");
        assert!(red.contains(REDACTED));
        assert_eq!(unredact(&red, &text).unwrap(), text);
    }

    #[test]
    fn github_app_private_key_from_a_file_reference() {
        struct Files;
        impl sourcegull_config::refs::RefSource for Files {
            fn env(&self, _: &str) -> Option<String> {
                None
            }
            fn file(&self, path: &str) -> std::io::Result<String> {
                assert_eq!(path, "/run/secrets/github-app.pem");
                Ok(APP_KEY.to_owned())
            }
        }
        let mut value =
            parse_jsonc(&app_config("${file:/run/secrets/github-app.pem}", "")).unwrap();
        let resolved = sourcegull_config::refs::resolve_refs(&mut value, &Files).unwrap();
        assert!(!resolved.is_empty());
        let c = validate_connection(&value.to_string()).unwrap().config;
        // File references drop trailing newlines; the key still parses.
        assert_eq!(c.github_app.unwrap().private_key, APP_KEY.trim_end());
    }
}
