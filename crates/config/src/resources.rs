//! The resources file (`SOURCEGULL_RESOURCES_FILE`): objects that are otherwise created in the UI,
//! declared in a JSONC file kept in git: code host connections, search contexts, roles, site
//! admins, outbound webhooks, executor secrets, code intelligence policies and feature flags.
//!
//! Every entry has a key that identifies it across edits: `key` when given, else its name.
//! Renaming the key recreates the object; changing anything else updates it. Strings can hold
//! secret references ([`crate::refs`]). Each section is optional: a section that is present
//! owns the objects it lists (and, with `exclusive`, every object of its kind); an absent
//! section manages nothing.
//!
//! [`parse_resources`] checks the format, keys and values that need no other context. The
//! checks the UI applies to each kind (code host configurations, context queries, webhook URLs,
//! policy patterns, …) run in the reconciler and `sourcegull config check`.

use std::collections::{BTreeMap, HashSet};
use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::refs::{RefSource, Resolved, resolve_refs};
use crate::{ConfigIssue, parse_jsonc};

/// A kind of object the resources file declares.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ResourceKind {
    SiteAdmins,
    Connections,
    SearchContexts,
    Roles,
    OutboundWebhooks,
    ExecutorSecrets,
    CodeIntelPolicies,
    FeatureFlags,
}

impl ResourceKind {
    /// Every kind, in the order they are applied (connections before the search contexts that
    /// name their repositories).
    pub const ALL: [ResourceKind; 8] = [
        Self::Connections,
        Self::SearchContexts,
        Self::Roles,
        Self::SiteAdmins,
        Self::OutboundWebhooks,
        Self::ExecutorSecrets,
        Self::CodeIntelPolicies,
        Self::FeatureFlags,
    ];

    /// The section name in the file, also stored in the database.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SiteAdmins => "siteAdmins",
            Self::Connections => "connections",
            Self::SearchContexts => "searchContexts",
            Self::Roles => "roles",
            Self::OutboundWebhooks => "outboundWebhooks",
            Self::ExecutorSecrets => "executorSecrets",
            Self::CodeIntelPolicies => "codeIntelPolicies",
            Self::FeatureFlags => "featureFlags",
        }
    }

    /// The kind for a section name.
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.as_str() == s)
    }
}

impl fmt::Display for ResourceKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The document, as written (before secret references are resolved, so the schema describes
/// the file people edit).
#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[schemars(title = "Sourcegull resources file")]
pub struct ResourcesFile {
    /// Editor schema URL; ignored.
    #[serde(rename = "$schema", default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    /// Each section present also owns the objects of its kind that the file does not list:
    /// they become read-only and nothing of that kind can be created in the UI.
    #[serde(default)]
    pub exclusive: bool,
    /// Email addresses of site admins. A user becomes site admin when one of their verified
    /// email addresses is listed; removing it takes back what the file granted. While this is
    /// set, the first user to sign up does not become site admin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub site_admins: Option<Vec<String>>,
    /// Code host connections.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connections: Option<Vec<ConnectionDecl>>,
    /// Instance-level search contexts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search_contexts: Option<Vec<SearchContextDecl>>,
    /// Custom roles and the users who have them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub roles: Option<Vec<RoleDecl>>,
    /// Outbound webhooks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outbound_webhooks: Option<Vec<OutboundWebhookDecl>>,
    /// Global executor secrets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executor_secrets: Option<Vec<ExecutorSecretDecl>>,
    /// Global code intelligence configuration policies (indexing and retention).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_intel_policies: Option<Vec<CodeIntelPolicyDecl>>,
    /// Feature flags.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feature_flags: Option<Vec<FeatureFlagDecl>>,
}

/// A code host kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum CodeHostKind {
    Gitlab,
    Github,
    Gitea,
}

impl CodeHostKind {
    /// The stored kind (`external_services.kind`).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Gitlab => "GITLAB",
            Self::Github => "GITHUB",
            Self::Gitea => "GITEA",
        }
    }
}

/// A code host connection.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConnectionDecl {
    /// Stable id of the connection; renaming it recreates the connection (and re-clones).
    pub key: String,
    pub kind: CodeHostKind,
    pub display_name: String,
    /// The connection configuration, as in the UI's editor.
    pub config: Value,
}

/// A search context.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SearchContextDecl {
    /// Stable id; defaults to `name`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// The name used in `context:`.
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Visible to every user (default); private instance contexts are visible to site admins.
    #[serde(default = "yes")]
    pub public: bool,
    /// Repository filters defining the context, e.g. `repo:^gitlab\.example\.com/acme/`.
    /// Exactly one of `query` and `repositories`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    /// Repositories and the revisions searched in them. Repositories that are not synced yet
    /// are added once they are.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repositories: Option<Vec<ContextRepoDecl>>,
}

/// A repository of a search context.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContextRepoDecl {
    /// The repository name, e.g. `gitlab.example.com/acme/api`.
    pub repository: String,
    /// Revisions to search; the default branch when empty.
    #[serde(default)]
    pub revisions: Vec<String>,
}

/// A custom role.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RoleDecl {
    /// Stable id; defaults to `name`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    pub name: String,
    /// Permissions as `NAMESPACE#ACTION`, e.g. `BATCH_CHANGES#WRITE`.
    #[serde(default)]
    pub permissions: Vec<String>,
    /// Email addresses of the users who have the role (matched against verified emails).
    /// Removing one takes back what the file granted.
    #[serde(default)]
    pub users: Vec<String>,
}

/// An outbound webhook.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OutboundWebhookDecl {
    /// Stable id of the webhook.
    pub key: String,
    pub url: String,
    /// The key payloads are signed with (HMAC-SHA256).
    pub secret: String,
    pub event_types: Vec<EventTypeDecl>,
}

/// An event type, optionally limited to one scope.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(untagged)]
pub enum EventTypeDecl {
    /// Every scope.
    Name(String),
    Scoped {
        #[serde(rename = "eventType")]
        event_type: String,
        scope: Option<String>,
    },
}

impl EventTypeDecl {
    /// The event type and scope.
    pub fn parts(&self) -> (&str, Option<&str>) {
        match self {
            Self::Name(n) => (n, None),
            Self::Scoped { event_type, scope } => (event_type, scope.as_deref()),
        }
    }
}

/// The jobs an executor secret is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ExecutorSecretScopeDecl {
    Batches,
    Codeintel,
}

/// A global executor secret.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExecutorSecretDecl {
    /// Stable id; defaults to `<scope>/<name>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    pub scope: ExecutorSecretScopeDecl,
    /// The environment variable name the jobs see, e.g. `GITLAB_TOKEN`.
    pub name: String,
    pub value: String,
}

/// The git objects a code intelligence policy matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PolicyObjectType {
    GitCommit,
    GitTree,
    GitTag,
}

/// A global code intelligence configuration policy.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CodeIntelPolicyDecl {
    /// Stable id of the policy.
    pub key: String,
    pub name: String,
    /// `GIT_TREE` (branches), `GIT_TAG` or `GIT_COMMIT`.
    #[serde(rename = "type")]
    pub object_type: PolicyObjectType,
    /// Branch, tag or commit pattern, e.g. `main` or `release/*`.
    pub pattern: String,
    /// Repository name globs; every repository when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository_patterns: Option<Vec<String>>,
    #[serde(default)]
    pub retention_enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention_duration_hours: Option<i32>,
    #[serde(default)]
    pub retain_intermediate_commits: bool,
    #[serde(default)]
    pub indexing_enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index_commit_max_age_hours: Option<i32>,
    #[serde(default)]
    pub index_intermediate_commits: bool,
    #[serde(default)]
    pub syntactic_indexing_enabled: bool,
}

/// A feature flag.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FeatureFlagDecl {
    /// Stable id; defaults to `name`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    pub name: String,
    /// The same value for everyone. Exactly one of `value` and `rolloutBasisPoints`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<bool>,
    /// On for this share of users, in basis points (0 to 10000).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rollout_basis_points: Option<i32>,
}

fn yes() -> bool {
    true
}

/// One declared entry, resolved and checked.
#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    Connection(ConnectionDecl),
    SearchContext(SearchContextDecl),
    Role(RoleDecl),
    OutboundWebhook(OutboundWebhookDecl),
    ExecutorSecret(ExecutorSecretDecl),
    CodeIntelPolicy(CodeIntelPolicyDecl),
    FeatureFlag(FeatureFlagDecl),
}

/// An entry with its key and the hash of its resolved contents (a changed hash means the
/// object needs an update).
#[derive(Debug, Clone, PartialEq)]
pub struct Declared {
    pub key: String,
    pub entry: Entry,
    pub hash: [u8; 32],
}

/// A parsed, resolved and checked resources file.
#[derive(Debug, Clone, Default)]
pub struct Resources {
    pub exclusive: bool,
    /// Lower-cased site admin emails, when the section is present.
    pub site_admins: Option<Vec<String>>,
    /// The entries of each present section (site admins aside), in file order.
    pub sections: BTreeMap<ResourceKind, Vec<Declared>>,
    /// Hash of the whole resolved document.
    pub hash: [u8; 32],
    /// The resolved secrets, to keep them out of messages.
    pub resolved: Resolved,
}

impl Resources {
    /// The kinds the file manages (its present sections).
    pub fn kinds(&self) -> Vec<ResourceKind> {
        let mut kinds: Vec<ResourceKind> = self.sections.keys().copied().collect();
        if self.site_admins.is_some() {
            kinds.push(ResourceKind::SiteAdmins);
        }
        kinds.sort();
        kinds
    }

    /// The entries of `kind`; `None` when its section is absent.
    pub fn section(&self, kind: ResourceKind) -> Option<&[Declared]> {
        self.sections.get(&kind).map(Vec::as_slice)
    }
}

/// Parses `text`, resolves its secret references and checks every entry. The issues name the
/// entry (`connections[gitlab-main].config.token`) and never a resolved value.
pub fn parse_resources(text: &str, refs: &dyn RefSource) -> Result<Resources, Vec<ConfigIssue>> {
    let raw = parse_jsonc(text).map_err(|e| {
        vec![ConfigIssue {
            pointer: String::new(),
            message: format!("syntax error: {e}"),
        }]
    })?;
    // The shape is checked before resolving, so a typo is reported as such rather than as a
    // missing variable in a field that would be rejected anyway.
    let file: ResourcesFile = match serde_json::from_value(raw.clone()) {
        Ok(f) => f,
        Err(_) => return Err(locate(&raw, shape_issues(&raw))),
    };
    let mut resolved_value = raw.clone();
    let resolved =
        resolve_refs(&mut resolved_value, refs).map_err(|issues| locate(&raw, issues))?;
    let file_resolved: ResourcesFile =
        serde_json::from_value(resolved_value.clone()).map_err(|e| {
            vec![ConfigIssue {
                pointer: String::new(),
                message: resolved.redact(&e.to_string()),
            }]
        })?;
    let mut issues = Vec::new();
    let mut out = Resources {
        exclusive: file.exclusive,
        hash: hash_value(&resolved_value),
        ..Default::default()
    };
    if let Some(admins) = &file_resolved.site_admins {
        let mut list = Vec::new();
        for (i, email) in admins.iter().enumerate() {
            let e = email.trim().to_lowercase();
            if !looks_like_email(&e) {
                issues.push(issue(
                    format!("/siteAdmins/{i}"),
                    format!("{email:?} is not an email address"),
                ));
            } else if !list.contains(&e) {
                list.push(e);
            }
        }
        if file.exclusive && list.is_empty() {
            issues.push(issue(
                "/siteAdmins",
                "must list at least one site admin when `exclusive` is set",
            ));
        }
        out.site_admins = Some(list);
    }
    let r = &file_resolved;
    section(
        &mut out,
        &mut issues,
        ResourceKind::Connections,
        r.connections.as_deref(),
        |c| (Some(c.key.clone()), Entry::Connection(c.clone())),
    );
    section(
        &mut out,
        &mut issues,
        ResourceKind::SearchContexts,
        r.search_contexts.as_deref(),
        |c| {
            (
                Some(c.key.clone().unwrap_or_else(|| c.name.clone())),
                Entry::SearchContext(c.clone()),
            )
        },
    );
    section(
        &mut out,
        &mut issues,
        ResourceKind::Roles,
        r.roles.as_deref(),
        |c| {
            (
                Some(c.key.clone().unwrap_or_else(|| c.name.clone())),
                Entry::Role(c.clone()),
            )
        },
    );
    section(
        &mut out,
        &mut issues,
        ResourceKind::OutboundWebhooks,
        r.outbound_webhooks.as_deref(),
        |c| (Some(c.key.clone()), Entry::OutboundWebhook(c.clone())),
    );
    section(
        &mut out,
        &mut issues,
        ResourceKind::ExecutorSecrets,
        r.executor_secrets.as_deref(),
        |c| {
            let natural = format!("{}/{}", scope_str(c.scope), c.name);
            (
                Some(c.key.clone().unwrap_or(natural)),
                Entry::ExecutorSecret(c.clone()),
            )
        },
    );
    section(
        &mut out,
        &mut issues,
        ResourceKind::CodeIntelPolicies,
        r.code_intel_policies.as_deref(),
        |c| (Some(c.key.clone()), Entry::CodeIntelPolicy(c.clone())),
    );
    section(
        &mut out,
        &mut issues,
        ResourceKind::FeatureFlags,
        r.feature_flags.as_deref(),
        |c| {
            (
                Some(c.key.clone().unwrap_or_else(|| c.name.clone())),
                Entry::FeatureFlag(c.clone()),
            )
        },
    );
    for (kind, entries) in &out.sections {
        check_section(*kind, entries, &mut issues);
    }
    if issues.is_empty() {
        out.resolved = resolved;
        Ok(out)
    } else {
        resolved.redact_issues(&mut issues);
        Err(locate(&raw, issues))
    }
}

/// The JSON Schema of the resources file, for editor completion.
pub fn resources_schema() -> Value {
    serde_json::to_value(schemars::schema_for!(ResourcesFile)).unwrap_or(Value::Bool(true))
}

/// `executorSecrets[].scope` as stored.
pub const fn scope_str(scope: ExecutorSecretScopeDecl) -> &'static str {
    match scope {
        ExecutorSecretScopeDecl::Batches => "batches",
        ExecutorSecretScopeDecl::Codeintel => "codeintel",
    }
}

fn section<T>(
    out: &mut Resources,
    issues: &mut Vec<ConfigIssue>,
    kind: ResourceKind,
    items: Option<&[T]>,
    entry: impl Fn(&T) -> (Option<String>, Entry),
) {
    let Some(items) = items else { return };
    let mut seen = HashSet::new();
    let mut list = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        let (key, entry) = entry(item);
        let key = key.unwrap_or_default();
        if !valid_key(&key) {
            issues.push(issue(
                format!("/{kind}/{i}/key"),
                format!(
                    "key {key:?} must be 1 to 200 characters of letters, digits, `.`, `_`, `-` \
                     and `/`"
                ),
            ));
            continue;
        }
        if !seen.insert(key.clone()) {
            issues.push(issue(
                format!("/{kind}/{i}/key"),
                format!("duplicate key {key:?}"),
            ));
            continue;
        }
        let hash = hash_entry(kind, &entry);
        list.push(Declared { key, entry, hash });
    }
    out.sections.insert(kind, list);
}

/// Checks that need no database.
fn check_section(kind: ResourceKind, entries: &[Declared], issues: &mut Vec<ConfigIssue>) {
    let mut names = HashSet::new();
    for (i, d) in entries.iter().enumerate() {
        let at = |field: &str| {
            if field.is_empty() {
                format!("/{kind}/{i}")
            } else {
                format!("/{kind}/{i}/{field}")
            }
        };
        let mut unique = |name: String, field: &str, issues: &mut Vec<ConfigIssue>| {
            if !names.insert(name.clone()) {
                issues.push(issue(at(field), format!("{name:?} is declared twice")));
            }
        };
        match &d.entry {
            Entry::Connection(c) => {
                if c.display_name.trim().is_empty() {
                    issues.push(issue(at("displayName"), "must not be empty"));
                }
                if !c.config.is_object() {
                    issues.push(issue(at("config"), "must be an object"));
                }
            }
            Entry::SearchContext(c) => {
                unique(c.name.to_lowercase(), "name", issues);
                match (&c.query, &c.repositories) {
                    (Some(q), None) if !q.trim().is_empty() => {}
                    (None, Some(_)) => {}
                    _ => issues.push(issue(
                        at(""),
                        "set exactly one of `query` and `repositories`",
                    )),
                }
            }
            Entry::Role(r) => {
                if r.name.trim().is_empty() {
                    issues.push(issue(at("name"), "must not be empty"));
                }
                if r.name == "USER" || r.name == "SITE_ADMINISTRATOR" {
                    issues.push(issue(
                        at("name"),
                        "system roles cannot be declared (use `siteAdmins` for site admins)",
                    ));
                }
                unique(r.name.clone(), "name", issues);
                for (j, p) in r.permissions.iter().enumerate() {
                    if p.split_once('#')
                        .is_none_or(|(n, a)| n.is_empty() || a.is_empty())
                    {
                        issues.push(issue(
                            at(&format!("permissions/{j}")),
                            format!("{p:?} is not of the form NAMESPACE#ACTION"),
                        ));
                    }
                }
                for (j, u) in r.users.iter().enumerate() {
                    if !looks_like_email(&u.trim().to_lowercase()) {
                        issues.push(issue(
                            at(&format!("users/{j}")),
                            format!("{u:?} is not an email address"),
                        ));
                    }
                }
            }
            Entry::OutboundWebhook(w) => {
                if !(w.url.starts_with("https://") || w.url.starts_with("http://")) {
                    issues.push(issue(at("url"), "must be an http or https URL"));
                }
                if w.secret.is_empty() {
                    issues.push(issue(at("secret"), "must not be empty"));
                }
                if w.event_types.is_empty() {
                    issues.push(issue(at("eventTypes"), "must list at least one event type"));
                }
            }
            Entry::ExecutorSecret(s) => {
                unique(format!("{}/{}", scope_str(s.scope), s.name), "name", issues);
            }
            Entry::CodeIntelPolicy(p) => {
                if p.name.trim().is_empty() {
                    issues.push(issue(at("name"), "must not be empty"));
                }
                if p.pattern.trim().is_empty() {
                    issues.push(issue(at("pattern"), "must not be empty"));
                }
                for (field, v) in [
                    ("retentionDurationHours", p.retention_duration_hours),
                    ("indexCommitMaxAgeHours", p.index_commit_max_age_hours),
                ] {
                    if v.is_some_and(|h| h < 0) {
                        issues.push(issue(at(field), "must not be negative"));
                    }
                }
            }
            Entry::FeatureFlag(f) => {
                unique(f.name.clone(), "name", issues);
                if f.name.trim().is_empty() {
                    issues.push(issue(at("name"), "must not be empty"));
                }
                match (f.value, f.rollout_basis_points) {
                    (Some(_), None) => {}
                    (None, Some(r)) if (0..=10_000).contains(&r) => {}
                    (None, Some(_)) => issues.push(issue(
                        at("rolloutBasisPoints"),
                        "must be between 0 and 10000",
                    )),
                    _ => issues.push(issue(
                        at(""),
                        "set exactly one of `value` and `rolloutBasisPoints`",
                    )),
                }
            }
        }
    }
}

/// Issues for a document that does not match [`ResourcesFile`], one per offending entry.
fn shape_issues(raw: &Value) -> Vec<ConfigIssue> {
    let Some(obj) = raw.as_object() else {
        return vec![issue("", "the resources file must be a JSON object")];
    };
    let mut issues = Vec::new();
    for (k, v) in obj {
        let check = |v: &Value| -> Option<String> {
            match k.as_str() {
                "$schema" => serde_json::from_value::<String>(v.clone())
                    .err()
                    .map(|e| e.to_string()),
                "exclusive" => serde_json::from_value::<bool>(v.clone())
                    .err()
                    .map(|e| e.to_string()),
                "siteAdmins" => serde_json::from_value::<Vec<String>>(v.clone())
                    .err()
                    .map(|e| e.to_string()),
                _ => None,
            }
        };
        let kind = ResourceKind::parse(k).filter(|k| *k != ResourceKind::SiteAdmins);
        match (k.as_str(), kind) {
            ("$schema" | "exclusive" | "siteAdmins", _) => {
                if let Some(e) = check(v) {
                    issues.push(issue(format!("/{k}"), e));
                }
            }
            (_, Some(kind)) => {
                let Some(items) = v.as_array() else {
                    issues.push(issue(format!("/{k}"), "must be an array"));
                    continue;
                };
                for (i, item) in items.iter().enumerate() {
                    if let Err(e) = entry_shape(kind, item) {
                        issues.push(issue(format!("/{k}/{i}"), e.to_string()));
                    }
                }
            }
            _ => issues.push(issue(
                format!("/{k}"),
                format!(
                    "unknown section; expected one of exclusive, {}",
                    ResourceKind::ALL.map(ResourceKind::as_str).join(", ")
                ),
            )),
        }
    }
    if issues.is_empty() {
        issues.push(issue(
            "",
            "the document does not match the resources file format",
        ));
    }
    issues
}

fn entry_shape(kind: ResourceKind, item: &Value) -> Result<(), serde_json::Error> {
    let v = item.clone();
    match kind {
        ResourceKind::Connections => serde_json::from_value::<ConnectionDecl>(v).map(drop),
        ResourceKind::SearchContexts => serde_json::from_value::<SearchContextDecl>(v).map(drop),
        ResourceKind::Roles => serde_json::from_value::<RoleDecl>(v).map(drop),
        ResourceKind::OutboundWebhooks => {
            serde_json::from_value::<OutboundWebhookDecl>(v).map(drop)
        }
        ResourceKind::ExecutorSecrets => serde_json::from_value::<ExecutorSecretDecl>(v).map(drop),
        ResourceKind::CodeIntelPolicies => {
            serde_json::from_value::<CodeIntelPolicyDecl>(v).map(drop)
        }
        ResourceKind::FeatureFlags => serde_json::from_value::<FeatureFlagDecl>(v).map(drop),
        ResourceKind::SiteAdmins => serde_json::from_value::<String>(v).map(drop),
    }
}

/// Rewrites `/connections/0/config/token` as `connections[gitlab-main].config.token`, taking
/// the entry's key from the raw document.
fn locate(raw: &Value, issues: Vec<ConfigIssue>) -> Vec<ConfigIssue> {
    issues
        .into_iter()
        .map(|i| ConfigIssue {
            pointer: describe_pointer(raw, &i.pointer),
            message: i.message,
        })
        .collect()
}

/// A readable location for a JSON pointer into the resources file.
pub fn describe_pointer(raw: &Value, pointer: &str) -> String {
    let parts: Vec<String> = pointer
        .split('/')
        .skip(1)
        .map(|p| p.replace("~1", "/").replace("~0", "~"))
        .collect();
    let Some((section, rest)) = parts.split_first() else {
        return pointer.to_owned();
    };
    let mut out = section.clone();
    let mut rest = rest.iter();
    if let Some(index) = rest.next() {
        let entry = index
            .parse::<usize>()
            .ok()
            .and_then(|i| raw.get(section).and_then(|s| s.get(i)));
        let label = entry
            .and_then(|e| {
                ["key", "name"]
                    .iter()
                    .find_map(|f| e.get(*f).and_then(Value::as_str))
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| index.clone());
        out.push_str(&format!("[{label}]"));
    }
    for p in rest {
        if p.parse::<usize>().is_ok() {
            out.push_str(&format!("[{p}]"));
        } else {
            out.push('.');
            out.push_str(p);
        }
    }
    out
}

fn issue(pointer: impl Into<String>, message: impl Into<String>) -> ConfigIssue {
    ConfigIssue {
        pointer: pointer.into(),
        message: message.into(),
    }
}

fn valid_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 200
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/'))
}

fn looks_like_email(s: &str) -> bool {
    s.split_once('@')
        .is_some_and(|(l, d)| !l.is_empty() && d.contains('.') && !d.starts_with('.'))
        && !s.chars().any(char::is_whitespace)
}

fn hash_entry(kind: ResourceKind, entry: &Entry) -> [u8; 32] {
    let value = match entry {
        Entry::Connection(e) => serde_json::to_value(e),
        Entry::SearchContext(e) => serde_json::to_value(e),
        Entry::Role(e) => serde_json::to_value(e),
        Entry::OutboundWebhook(e) => serde_json::to_value(e),
        Entry::ExecutorSecret(e) => serde_json::to_value(e),
        Entry::CodeIntelPolicy(e) => serde_json::to_value(e),
        Entry::FeatureFlag(e) => serde_json::to_value(e),
    }
    .unwrap_or(Value::Null);
    let mut h = Sha256::new();
    h.update(kind.as_str());
    h.update([0]);
    canonical(&value, &mut h);
    h.finalize().into()
}

fn hash_value(value: &Value) -> [u8; 32] {
    let mut h = Sha256::new();
    canonical(value, &mut h);
    h.finalize().into()
}

/// Feeds `value` to `h` with object keys sorted, so the hash does not depend on key order.
fn canonical(value: &Value, h: &mut Sha256) {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            h.update(b"{");
            for k in keys {
                h.update(Value::String(k.clone()).to_string());
                h.update(b":");
                canonical(&map[k], h);
                h.update(b",");
            }
            h.update(b"}");
        }
        Value::Array(items) => {
            h.update(b"[");
            for item in items {
                canonical(item, h);
                h.update(b",");
            }
            h.update(b"]");
        }
        other => h.update(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct Env(HashMap<&'static str, &'static str>);

    impl RefSource for Env {
        fn env(&self, name: &str) -> Option<String> {
            self.0.get(name).map(|v| (*v).to_owned())
        }
        fn file(&self, _: &str) -> std::io::Result<String> {
            Err(std::io::ErrorKind::NotFound.into())
        }
    }

    fn env() -> Env {
        Env(HashMap::from([
            ("GITLAB_TOKEN", "glpat-1"),
            ("HOOK", "s3cret"),
        ]))
    }

    const FULL: &str = r#"{
        // Everything, once.
        "exclusive": false,
        "siteAdmins": ["Alice@Example.com", "alice@example.com"],
        "connections": [{
            "key": "gitlab-main", "kind": "gitlab", "displayName": "GitLab",
            "config": {"url": "https://gitlab.example.com", "token": "${env:GITLAB_TOKEN}"},
        }],
        "searchContexts": [
            {"name": "backend", "query": "repo:^gitlab\\.example\\.com/acme/"},
            {"name": "api", "repositories": [{"repository": "gitlab.example.com/acme/api", "revisions": ["main"]}]},
        ],
        "roles": [{"name": "bulk-editors", "permissions": ["BATCH_CHANGES#WRITE"], "users": ["bob@example.com"]}],
        "outboundWebhooks": [{"key": "audit", "url": "https://hooks.example.com/sourcegull", "secret": "${env:HOOK}",
            "eventTypes": ["batch_change:apply", {"eventType": "batch_change:close", "scope": null}]}],
        "executorSecrets": [{"scope": "batches", "name": "GITLAB_TOKEN", "value": "${env:GITLAB_TOKEN}"}],
        "codeIntelPolicies": [{"key": "main", "name": "Default branch", "type": "GIT_TREE", "pattern": "main", "indexingEnabled": true}],
        "featureFlags": [{"name": "new-ui", "value": true}, {"name": "beta", "rolloutBasisPoints": 2500}],
    }"#;

    #[test]
    fn parses_every_section() {
        let r = parse_resources(FULL, &env()).unwrap();
        assert_eq!(
            r.site_admins.as_deref(),
            Some(&["alice@example.com".to_owned()][..])
        );
        assert_eq!(r.kinds().len(), 8);
        let conns = r.section(ResourceKind::Connections).unwrap();
        assert_eq!(conns[0].key, "gitlab-main");
        let Entry::Connection(c) = &conns[0].entry else {
            panic!()
        };
        assert_eq!(c.config["token"], "glpat-1");
        let contexts = r.section(ResourceKind::SearchContexts).unwrap();
        assert_eq!(contexts[1].key, "api");
        let secrets = r.section(ResourceKind::ExecutorSecrets).unwrap();
        assert_eq!(secrets[0].key, "batches/GITLAB_TOKEN");
        assert!(r.section(ResourceKind::FeatureFlags).unwrap().len() == 2);
    }

    #[test]
    fn absent_sections_manage_nothing() {
        let r = parse_resources(r#"{"featureFlags": []}"#, &env()).unwrap();
        assert_eq!(r.kinds(), [ResourceKind::FeatureFlags]);
        assert!(r.section(ResourceKind::Connections).is_none());
        assert!(r.site_admins.is_none());
        assert!(parse_resources("", &env()).unwrap().kinds().is_empty());
    }

    #[test]
    fn hashes_follow_resolved_contents_not_formatting() {
        let a = parse_resources(FULL, &env()).unwrap();
        let reordered = FULL.replace(
            r#""key": "gitlab-main", "kind": "gitlab""#,
            r#""kind": "gitlab", "key": "gitlab-main""#,
        );
        let b = parse_resources(&reordered, &env()).unwrap();
        let first = |r: &Resources| r.section(ResourceKind::Connections).unwrap()[0].hash;
        assert_eq!(first(&a), first(&b));
        let rotated = Env(HashMap::from([
            ("GITLAB_TOKEN", "glpat-2"),
            ("HOOK", "s3cret"),
        ]));
        let c = parse_resources(FULL, &rotated).unwrap();
        assert_ne!(first(&a), first(&c), "a rotated secret is a change");
        assert_ne!(a.hash, c.hash);
    }

    #[test]
    fn errors_name_entries_and_hide_secrets() {
        let text = r#"{
            "connections": [
                {"key": "gitlab-main", "kind": "gitlab", "displayName": "GitLab", "config": {"token": "${env:MISSING}"}},
            ],
        }"#;
        let issues = parse_resources(text, &env()).unwrap_err();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].pointer, "connections[gitlab-main].config.token");
        assert!(issues[0].message.contains("MISSING"));

        let text = r#"{
            "roles": [{"name": "x", "permissions": ["nope"], "users": ["${env:HOOK}"]}],
            "featureFlags": [{"name": "f"}, {"name": "f", "value": true}],
            "searchContexts": [{"name": "c", "query": "q", "repositories": []}],
            "outboundWebhooks": [{"key": "w", "url": "ftp://x", "secret": "s", "eventTypes": []}],
        }"#;
        let issues = parse_resources(text, &env()).unwrap_err();
        let text: Vec<String> = issues.iter().map(ToString::to_string).collect();
        assert!(
            text.iter()
                .any(|t| t.starts_with("roles[x].permissions[0]")),
            "{text:?}"
        );
        assert!(
            text.iter().any(|t| t.starts_with("roles[x].users[0]")),
            "{text:?}"
        );
        assert!(text.iter().all(|t| !t.contains("s3cret")), "{text:?}");
        assert!(
            text.iter().any(|t| t.contains("duplicate key \"f\"")),
            "{text:?}"
        );
        assert!(
            text.iter().any(|t| t.contains("exactly one of `value`")),
            "{text:?}"
        );
        assert!(
            text.iter().any(|t| t.contains("exactly one of `query`")),
            "{text:?}"
        );
        assert!(
            text.iter()
                .any(|t| t.starts_with("outboundWebhooks[w].url")),
            "{text:?}"
        );
        assert!(
            text.iter()
                .any(|t| t.starts_with("outboundWebhooks[w].eventTypes")),
            "{text:?}"
        );
    }

    #[test]
    fn shape_errors_point_at_the_entry() {
        let issues = parse_resources(
            r#"{"connections": [{"key": "a", "kind": "gitlab", "displayName": "A", "config": {}, "typo": 1}], "nope": []}"#,
            &env(),
        )
        .unwrap_err();
        let text: Vec<String> = issues.iter().map(ToString::to_string).collect();
        assert!(
            text.iter()
                .any(|t| t.starts_with("connections[a]: ") && t.contains("typo")),
            "{text:?}"
        );
        assert!(
            text.iter().any(|t| t.starts_with("nope: unknown section")),
            "{text:?}"
        );
    }

    #[test]
    fn keys_are_checked() {
        let issues = parse_resources(
            r#"{"featureFlags": [{"key": "a b", "name": "x", "value": true}, {"key": "k", "name": "y", "value": true}, {"key": "k", "name": "z", "value": true}]}"#,
            &env(),
        )
        .unwrap_err();
        assert_eq!(issues.len(), 2, "{issues:?}");
    }

    #[test]
    fn exclusive_site_admins_cannot_be_empty() {
        let issues =
            parse_resources(r#"{"exclusive": true, "siteAdmins": []}"#, &env()).unwrap_err();
        assert!(issues[0].message.contains("at least one"));
        assert!(parse_resources(r#"{"siteAdmins": []}"#, &env()).is_ok());
    }

    #[test]
    fn schema_lists_the_sections() {
        let schema = resources_schema();
        let props = &schema["properties"];
        for kind in ResourceKind::ALL {
            assert!(props.get(kind.as_str()).is_some(), "{kind}");
        }
    }
}
