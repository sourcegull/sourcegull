//! `sourcegull config check`: validates the configuration files without a database, as the server
//! would load them: the site config file (`SOURCEGULL_SITE_CONFIG_FILE`), the resources file
//! (`SOURCEGULL_RESOURCES_FILE`) and the global settings file (`SOURCEGULL_GLOBAL_SETTINGS_FILE`).
//! Secret references are resolved from the environment the command runs in; missing ones are
//! reported by name, values are never printed.

use std::fmt::Write as _;
use std::path::Path;

use sourcegull_config::SiteConfig;
use sourcegull_config::refs::ProcessRefs;
use sourcegull_config::resources::ResourceKind;
use sourcegull_repos::Registry;

/// The report and whether every file passed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckOutcome {
    pub output: String,
    pub success: bool,
}

/// Checks the given files; at least one should be given.
pub fn check(
    site_config: Option<&Path>,
    resources: Option<&Path>,
    global_settings: Option<&Path>,
) -> CheckOutcome {
    let mut out = String::new();
    let mut success = true;
    if site_config.is_none() && resources.is_none() && global_settings.is_none() {
        return CheckOutcome {
            output: "nothing to check: pass --site-config, --resources or --global-settings \
                     (or set SOURCEGULL_SITE_CONFIG_FILE, SOURCEGULL_RESOURCES_FILE, \
                     SOURCEGULL_GLOBAL_SETTINGS_FILE)"
                .to_owned(),
            success: false,
        };
    }
    // The resources file's webhook URLs are checked against the site config's allowed hosts.
    let mut site = SiteConfig::default();
    if let Some(path) = site_config {
        match std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("reading the file: {e}"))
            .and_then(|text| crate::site_config::resolve_file(&text))
            .and_then(|(value, refs)| crate::site_config::validate_resolved(&value, &refs))
        {
            Ok(validated) => {
                let _ = writeln!(out, "site config {}: ok", path.display());
                for w in &validated.warnings {
                    let _ = writeln!(out, "  warning: {w}");
                }
                site = validated.config;
            }
            Err(e) => {
                success = false;
                let _ = writeln!(out, "site config {}: invalid", path.display());
                for line in format!("{e:#}").lines() {
                    let _ = writeln!(out, "  {line}");
                }
            }
        }
    }
    if let Some(path) = resources {
        let checked = std::fs::read_to_string(path)
            .map_err(|e| format!("reading the file: {e}"))
            .and_then(|text| {
                sourcegull_declarative::check_resources(
                    &text,
                    &ProcessRefs,
                    &Registry::default(),
                    &site,
                )
                .map_err(|issues| sourcegull_declarative::describe_issues(&issues))
            });
        match checked {
            Ok(c) => {
                let r = &c.resources;
                let mut parts = Vec::new();
                for kind in r.kinds() {
                    let n = match kind {
                        ResourceKind::SiteAdmins => r.site_admins.as_ref().map_or(0, Vec::len),
                        k => r.section(k).map_or(0, <[_]>::len),
                    };
                    parts.push(format!("{n} {kind}"));
                }
                let _ = writeln!(
                    out,
                    "resources {}: ok ({}{})",
                    path.display(),
                    if parts.is_empty() {
                        "no sections".to_owned()
                    } else {
                        parts.join(", ")
                    },
                    if r.exclusive { ", exclusive" } else { "" }
                );
                for w in &c.warnings {
                    let _ = writeln!(out, "  warning: {w}");
                }
            }
            Err(e) => {
                success = false;
                let _ = writeln!(out, "resources {}: invalid", path.display());
                for line in e.lines() {
                    let _ = writeln!(out, "  {line}");
                }
            }
        }
    }
    if let Some(path) = global_settings {
        let checked = std::fs::read_to_string(path)
            .map_err(|e| format!("reading the file: {e}"))
            .and_then(|text| {
                sourcegull_declarative::check_global_settings(&text)
                    .map_err(|issues| sourcegull_declarative::describe_issues(&issues))
            });
        match checked {
            Ok(()) => {
                let _ = writeln!(out, "global settings {}: ok", path.display());
            }
            Err(e) => {
                success = false;
                let _ = writeln!(out, "global settings {}: invalid", path.display());
                for line in e.lines() {
                    let _ = writeln!(out, "  {line}");
                }
            }
        }
    }
    CheckOutcome {
        output: out.trim_end().to_owned(),
        success,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, text: &str) -> std::path::PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, text).unwrap();
        p
    }

    #[test]
    fn checks_each_file() {
        let dir =
            std::env::temp_dir().join(format!("sourcegull-config-check-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let secret = write(&dir, "token", "glpat-secret-value\n");
        let site = write(
            &dir,
            "site.json",
            r#"{"externalURL": "https://sourcegull.example.com"}"#,
        );
        let resources = write(
            &dir,
            "resources.jsonc",
            &format!(
                r#"{{
                    "siteAdmins": ["admin@example.com"],
                    "connections": [{{"key": "gl", "kind": "gitlab", "displayName": "GitLab",
                        "config": {{"url": "https://gitlab.example.com", "token": "${{file:{}}}", "projectQuery": ["none"]}}}}],
                }}"#,
                secret.display()
            ),
        );
        let settings = write(
            &dir,
            "settings.json",
            r#"{"search.defaultPatternType": "regexp"}"#,
        );
        let ok = check(Some(&site), Some(&resources), Some(&settings));
        assert!(ok.success, "{}", ok.output);
        assert!(
            ok.output.contains("1 siteAdmins, 1 connections"),
            "{}",
            ok.output
        );
        assert!(!ok.output.contains("glpat-secret-value"));

        std::fs::remove_file(&secret).unwrap();
        let bad_settings = write(&dir, "bad.json", "[]");
        let bad = check(None, Some(&resources), Some(&bad_settings));
        assert!(!bad.success);
        assert!(
            bad.output.contains("connections[gl].config.token"),
            "{}",
            bad.output
        );
        assert!(bad.output.contains("global settings"), "{}", bad.output);
        assert!(!check(None, None, None).success);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
