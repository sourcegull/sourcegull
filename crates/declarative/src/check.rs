//! Checks that need no database: the file format, secret references and each code host
//! connection's configuration. `sourcegull config check` runs them offline; the reconciler runs
//! them before applying anything.

use sourcegull_codeintel_policies::fields::check_policy_fields;
use sourcegull_codeintel_policies::store::GitObjectType;
use sourcegull_config::refs::RefSource;
use sourcegull_config::resources::{
    CodeHostKind, CodeIntelPolicyDecl, Entry, PolicyObjectType, ResourceKind, Resources,
    parse_resources,
};
use sourcegull_config::{
    AuthProvider, ConfigError, ConfigIssue, Settings, SiteConfig, parse_jsonc,
};
use sourcegull_executor_api::secrets;
use sourcegull_outbound_webhooks::events::is_known as is_known_webhook_event;
use sourcegull_outbound_webhooks::guard::check_webhook_url;
use sourcegull_repos::Registry;
use sourcegull_search::check_context_fields;
use sourcegull_types::ExternalServiceKind;

/// A resources file that passed every offline check.
#[derive(Debug, Clone)]
pub struct CheckedResources {
    pub resources: Resources,
    /// Non-fatal findings, such as unknown keys in a connection configuration.
    pub warnings: Vec<ConfigIssue>,
}

/// The stored kind of a declared connection.
pub fn connection_kind(kind: CodeHostKind) -> ExternalServiceKind {
    match kind {
        CodeHostKind::Gitlab => ExternalServiceKind::GitLab,
        CodeHostKind::Github => ExternalServiceKind::GitHub,
        CodeHostKind::Gitea => ExternalServiceKind::Gitea,
    }
}

/// The configuration text stored for a declared connection.
pub fn connection_config_text(config: &serde_json::Value) -> String {
    serde_json::to_string_pretty(config).unwrap_or_else(|_| "{}".to_owned())
}

/// Parses and checks a resources file with the checks the UI applies to the same objects.
/// `config` supplies `webhooks.outbound.allowedHosts`. Issues name the entry and never a
/// resolved secret.
pub fn check_resources(
    text: &str,
    refs: &dyn RefSource,
    registry: &Registry,
    config: &SiteConfig,
) -> Result<CheckedResources, Vec<ConfigIssue>> {
    let resources = parse_resources(text, refs)?;
    let mut issues = Vec::new();
    let mut warnings = Vec::new();
    for kind in ResourceKind::ALL {
        for d in resources.section(kind).unwrap_or_default() {
            for (field, message) in entry_problems(&d.entry, config) {
                let at = if field.is_empty() {
                    format!("{kind}[{}]", d.key)
                } else {
                    format!("{kind}[{}].{field}", d.key)
                };
                issues.push(ConfigIssue {
                    pointer: at,
                    message: resources.resolved.redact(&message),
                });
            }
        }
    }
    for d in resources
        .section(ResourceKind::Connections)
        .unwrap_or_default()
    {
        let Entry::Connection(c) = &d.entry else {
            continue;
        };
        let at = |pointer: &str| format!("connections[{}].config{}", d.key, dotted(pointer));
        let text = connection_config_text(&c.config);
        match registry.validate(connection_kind(c.kind), &text) {
            Ok(check) => warnings.extend(check.warnings.into_iter().map(|w| ConfigIssue {
                pointer: at(&w.pointer),
                message: resources.resolved.redact(&w.message),
            })),
            Err(ConfigError::Syntax(e)) => issues.push(ConfigIssue {
                pointer: at(""),
                message: resources.resolved.redact(&e.to_string()),
            }),
            Err(ConfigError::Invalid(list)) => {
                issues.extend(list.into_iter().map(|i| ConfigIssue {
                    pointer: at(&i.pointer),
                    message: resources.resolved.redact(&i.message),
                }));
            }
        }
    }
    if issues.is_empty() {
        Ok(CheckedResources {
            resources,
            warnings,
        })
    } else {
        Err(issues)
    }
}

/// The UI's checks of one entry, as `(field, message)`.
fn entry_problems(entry: &Entry, config: &SiteConfig) -> Vec<(&'static str, String)> {
    let all = |field: &'static str, list: Vec<String>| {
        list.into_iter()
            .map(move |m| (field, m))
            .collect::<Vec<_>>()
    };
    match entry {
        Entry::SearchContext(c) => all(
            "",
            check_context_fields(
                true,
                &c.name,
                &c.description,
                c.query.as_deref().unwrap_or(""),
                c.repositories.as_ref().map_or(0, Vec::len),
            ),
        ),
        Entry::ExecutorSecret(s) => {
            let mut out = Vec::new();
            if !secrets::valid_key(&s.name) {
                out.push((
                    "name",
                    "invalid key format, should be a valid env var name".to_owned(),
                ));
            }
            if let Err(e) = secrets::validate_value(&s.name, &s.value) {
                out.push(("value", e));
            }
            out
        }
        Entry::OutboundWebhook(w) => {
            let mut out = Vec::new();
            if let Err(e) = check_webhook_url(config, &w.url) {
                out.push(("url", e));
            }
            for e in &w.event_types {
                let (event_type, _) = e.parts();
                if !is_known_webhook_event(event_type) {
                    out.push(("eventTypes", format!("unknown event type {event_type:?}")));
                }
            }
            out
        }
        Entry::CodeIntelPolicy(p) => match policy_fields(p) {
            Ok(_) => Vec::new(),
            Err(list) => all("", list),
        },
        Entry::Connection(_) | Entry::Role(_) | Entry::FeatureFlag(_) => Vec::new(),
    }
}

/// A declared policy as stored, checked like the UI checks it.
pub fn policy_fields(
    p: &CodeIntelPolicyDecl,
) -> Result<sourcegull_codeintel_policies::store::PolicyFields, Vec<String>> {
    check_policy_fields(
        p.repository_patterns.clone(),
        &p.name,
        Some(match p.object_type {
            PolicyObjectType::GitCommit => GitObjectType::Commit,
            PolicyObjectType::GitTree => GitObjectType::Branch,
            PolicyObjectType::GitTag => GitObjectType::Tag,
        }),
        &p.pattern,
        p.retention_enabled,
        p.retention_duration_hours,
        p.retain_intermediate_commits,
        p.indexing_enabled,
        p.syntactic_indexing_enabled,
        p.index_commit_max_age_hours,
        p.index_intermediate_commits,
    )
}

/// Checks a global settings document; returns the text to store.
pub fn check_global_settings(text: &str) -> Result<(), Vec<ConfigIssue>> {
    let issue = |message: String| {
        vec![ConfigIssue {
            pointer: String::new(),
            message,
        }]
    };
    let value = parse_jsonc(text).map_err(|e| issue(format!("syntax error: {e}")))?;
    if !value.is_object() {
        return Err(issue("settings must be a JSON object".to_owned()));
    }
    Settings::from_value(value)
        .map(drop)
        .map_err(|e| issue(e.to_string()))
}

/// `/a/b/0` as `.a.b[0]`.
fn dotted(pointer: &str) -> String {
    let mut out = String::new();
    for p in pointer.split('/').skip(1) {
        let p = p.replace("~1", "/").replace("~0", "~");
        if p.parse::<usize>().is_ok() {
            out.push_str(&format!("[{p}]"));
        } else {
            out.push('.');
            out.push_str(&p);
        }
    }
    out
}

/// Checks that a declared site admin can get an account under `config`'s sign-in settings.
/// Declared admins are matched against verified emails, so `siteAdmins` needs a way to verify
/// one without an existing admin: an external provider (GitLab, GitHub, Gitea, OpenID
/// Connect, or `http-header` with `emailHeader`), SCIM provisioning (`scim.authToken`), or
/// open built-in sign-up (`allowSignup`) with email (`email.smtp`) to confirm the address.
/// Without one, no admin could ever sign in, so the file is refused.
pub fn check_site_admin_sign_in(resources: &Resources, config: &SiteConfig) -> Option<ConfigIssue> {
    if resources.site_admins.as_ref().is_none_or(Vec::is_empty) {
        return None;
    }
    let verifies = config.auth_providers.iter().any(|p| match p {
        AuthProvider::Gitlab(_)
        | AuthProvider::Github(_)
        | AuthProvider::Gitea(_)
        | AuthProvider::OpenIdConnect(_) => true,
        AuthProvider::HttpHeader(h) => h.email_header.is_some(),
        AuthProvider::Builtin(b) => b.allow_signup && config.email_smtp.is_some(),
    }) || config.scim_auth_token.is_some();
    (!verifies).then(|| ConfigIssue {
        pointer: "siteAdmins".to_owned(),
        message: "no sign-in method can verify a declared admin's email, so no admin could sign \
                  in: add a GitLab, GitHub, Gitea or OpenID Connect provider (or `http-header` \
                  with `emailHeader`) to `auth.providers`, set `scim.authToken`, or set \
                  `allowSignup` on the built-in provider together with `email.smtp`"
            .to_owned(),
    })
}

/// One line per issue, for errors and logs.
pub fn describe_issues(issues: &[ConfigIssue]) -> String {
    issues
        .iter()
        .map(|i| {
            if i.pointer.is_empty() {
                i.message.clone()
            } else {
                format!("{}: {}", i.pointer, i.message)
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NoRefs;

    impl RefSource for NoRefs {
        fn env(&self, name: &str) -> Option<String> {
            (name == "TOKEN").then(|| "glpat-x".to_owned())
        }
        fn file(&self, _: &str) -> std::io::Result<String> {
            Err(std::io::ErrorKind::NotFound.into())
        }
    }

    #[test]
    fn validates_connection_configs_with_entry_locations() {
        let ok = r#"{"connections": [{"key": "gl", "kind": "gitlab", "displayName": "GitLab",
            "config": {"url": "https://gitlab.example.com", "token": "${env:TOKEN}", "projectQuery": ["none"]}}]}"#;
        let config = SiteConfig::default();
        let checked = check_resources(ok, &NoRefs, &Registry::default(), &config).unwrap();
        assert_eq!(checked.resources.kinds(), [ResourceKind::Connections]);

        let bad = r#"{"connections": [{"key": "gl", "kind": "gitlab", "displayName": "GitLab",
            "config": {"url": 3, "token": "${env:TOKEN}"}}]}"#;
        let issues = check_resources(bad, &NoRefs, &Registry::default(), &config).unwrap_err();
        assert!(
            issues
                .iter()
                .all(|i| i.pointer.starts_with("connections[gl].config")),
            "{issues:?}"
        );
        assert!(
            issues.iter().all(|i| !i.message.contains("glpat-x")),
            "{issues:?}"
        );
    }

    #[test]
    fn applies_the_ui_checks() {
        let text = r#"{
            "searchContexts": [{"name": "global", "query": "repo:a"}],
            "executorSecrets": [{"scope": "batches", "name": "DOCKER_AUTH_CONFIG", "value": "nope"}],
            "outboundWebhooks": [{"key": "w", "url": "http://127.0.0.1/hook", "secret": "s", "eventTypes": ["no:such"]}],
            "codeIntelPolicies": [{"key": "p", "name": "P", "type": "GIT_TREE", "pattern": "main", "retentionDurationHours": 0}],
        }"#;
        let issues = check_resources(text, &NoRefs, &Registry::default(), &SiteConfig::default())
            .unwrap_err();
        let text: Vec<String> = issues.iter().map(ToString::to_string).collect();
        for expected in [
            "searchContexts[global]: ",
            "executorSecrets[batches/DOCKER_AUTH_CONFIG].value: ",
            "outboundWebhooks[w].eventTypes: ",
            "codeIntelPolicies[p]: ",
        ] {
            assert!(
                text.iter().any(|t| t.starts_with(expected)),
                "{expected}: {text:?}"
            );
        }
    }

    #[test]
    fn checks_global_settings() {
        assert!(check_global_settings(r#"{"search.defaultPatternType": "regexp"}"#).is_ok());
        assert!(check_global_settings("[]").is_err());
        assert!(check_global_settings("{").is_err());
    }

    #[test]
    fn dotted_pointers() {
        assert_eq!(dotted("/a/0/b~1c"), ".a[0].b/c");
        assert_eq!(dotted(""), "");
    }

    #[test]
    fn site_admins_need_a_way_to_verify_an_email() {
        let resources = |text: &str| parse_resources(text, &NoRefs).unwrap();
        let with = |providers: serde_json::Value| SiteConfig {
            auth_providers: serde_json::from_value(providers).unwrap(),
            ..SiteConfig::default()
        };
        let admins = resources(r#"{"siteAdmins": ["admin@example.com"]}"#);
        // Password sign-in alone (the default) cannot verify a declared admin's email.
        let builtin = SiteConfig::default();
        let issue = check_site_admin_sign_in(&admins, &builtin).unwrap();
        assert_eq!(issue.pointer, "siteAdmins");
        // Without declared admins, or with an empty list, nothing is required.
        assert!(check_site_admin_sign_in(&resources("{}"), &builtin).is_none());
        assert!(check_site_admin_sign_in(&resources(r#"{"siteAdmins": []}"#), &builtin).is_none());

        let gitlab = with(serde_json::json!([
            {"type": "builtin"},
            {"type": "gitlab", "clientID": "id", "clientSecret": "secret"}
        ]));
        assert!(check_site_admin_sign_in(&admins, &gitlab).is_none());
        let header = |email: bool| {
            let mut p = serde_json::json!({"type": "http-header", "usernameHeader": "X-User"});
            if email {
                p["emailHeader"] = "X-Email".into();
            }
            with(serde_json::json!([p]))
        };
        assert!(check_site_admin_sign_in(&admins, &header(false)).is_some());
        assert!(check_site_admin_sign_in(&admins, &header(true)).is_none());
        let scim = SiteConfig {
            scim_auth_token: Some("t".to_owned()),
            ..SiteConfig::default()
        };
        assert!(check_site_admin_sign_in(&admins, &scim).is_none());
        // Open sign-up verifies the address by email, so it needs SMTP.
        let open = with(serde_json::json!([{"type": "builtin", "allowSignup": true}]));
        assert!(check_site_admin_sign_in(&admins, &open).is_some());
        let open_smtp = SiteConfig {
            email_smtp: serde_json::from_value(serde_json::json!({"host": "smtp", "port": 25}))
                .unwrap(),
            ..open
        };
        assert!(check_site_admin_sign_in(&admins, &open_smtp).is_none());
    }
}
