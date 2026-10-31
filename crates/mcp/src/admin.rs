//! The part of the server instructions only site admins get: the tools cannot change the
//! instance, so this tells the agent where changes are made instead (the configuration files
//! when the deployment uses them, otherwise the GraphQL API with an access token kept in an
//! environment variable).

use sourcegull_services::{AppState, SiteConfigSource};

/// Where the user docs for changing settings with an agent are.
const DOCS_URL: &str = "https://github.com/sourcegull/sourcegull/blob/main/docs/configuration.md#changing-settings-with-an-agent";

/// Which configuration comes from files instead of the database.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Sources {
    /// The `SOURCEGULL_SITE_CONFIG_FILE` path.
    pub site_config_file: Option<String>,
    /// The `SOURCEGULL_GLOBAL_SETTINGS_FILE` path.
    pub global_settings_file: Option<String>,
    /// The `SOURCEGULL_RESOURCES_FILE` path and the sections it manages.
    pub resources_file: Option<(String, Vec<String>)>,
}

impl Sources {
    /// Reads the sources from the state; a database error counts as no file.
    pub async fn load(state: &AppState) -> Self {
        let declared = state.db.declared();
        let source = async |name| {
            declared.source(name).await.unwrap_or_else(|e| {
                tracing::warn!(error = %e, source = name, "reading a declared config source");
                None
            })
        };
        let global = source(sourcegull_db::GLOBAL_SETTINGS_SOURCE).await;
        let resources = source(sourcegull_db::RESOURCES_SOURCE).await;
        Self {
            site_config_file: match &state.site_config_source {
                SiteConfigSource::File(p) => Some(p.display().to_string()),
                SiteConfigSource::Database => None,
            },
            global_settings_file: global.map(|s| s.path),
            resources_file: resources.map(|s| (s.path, s.kinds)),
        }
    }
}

/// The instructions for a site admin; `base` is the instance's URL without a trailing `/`.
pub(crate) fn instructions(base: &str, s: &Sources) -> String {
    let mut out = String::from(
        "\n\nChanging this instance (the user is a site admin): these tools are read-only and \
         cannot change settings, users or code host connections. When the user asks for such \
         a change, use the route below, show the exact change first and save it only after \
         they confirm. Never ask the user to paste a token or secret into the conversation.",
    );
    let mut files = Vec::new();
    if let Some(p) = &s.site_config_file {
        files.push(format!(
            "the site configuration (SOURCEGULL_SITE_CONFIG_FILE, `{p}`)"
        ));
    }
    if let Some(p) = &s.global_settings_file {
        files.push(format!(
            "the global settings (SOURCEGULL_GLOBAL_SETTINGS_FILE, `{p}`)"
        ));
    }
    if let Some((p, kinds)) = &s.resources_file {
        let kinds = if kinds.is_empty() {
            String::new()
        } else {
            format!(": {}", kinds.join(", "))
        };
        files.push(format!(
            "resources (SOURCEGULL_RESOURCES_FILE, `{p}`{kinds})"
        ));
    }
    if !files.is_empty() {
        out.push_str(&format!(
            "\n\nThis deployment sets {} from files, which are read-only through the API: change \
             those in the files, usually kept in the deployment's git repository (propose the \
             edit or a merge request), and validate them with `sourcegull config check`.",
            files.join("; ")
        ));
    }
    out.push_str(&format!(
        "\n\n{}GraphQL API: `{base}/.api/graphql`. MCP sign-in tokens do not work there; ask the \
         user to create an access token under User settings → Access tokens \
         (`{base}/user/settings/tokens`) with a short expiry, and to export it as \
         SOURCEGULL_ADMIN_TOKEN in the shell your commands run in. Send \
         `Authorization: token $SOURCEGULL_ADMIN_TOKEN` and refer to the variable, never its \
         value. Each request is a POST of `{{\"query\": …, \"variables\": …}}`.",
        if files.is_empty() {
            ""
        } else {
            "Everything else goes through the "
        }
    ));
    if s.site_config_file.is_none() {
        out.push_str(
            "\nSite configuration: read `{ site { configuration { id effectiveContents } } }`, \
             then save the whole edited document with `mutation($id: Int!, $input: String!) { \
             updateSiteConfiguration(lastID: $id, input: $input) }`. Secrets show as \
             \"REDACTED\"; leaving them so keeps their stored value.",
        );
    }
    if s.global_settings_file.is_none() {
        out.push_str(
            "\nGlobal settings: read `{ site { id latestSettings { id contents } } }`, then save \
             with `mutation($subject: ID!, $id: Int, $contents: String!) { \
             settingsMutation(input: { subject: $subject, lastID: $id }) { \
             overwriteSettings(contents: $contents) { empty { alwaysNil } } } }`.",
        );
    }
    out.push_str(&format!(
        "\nThe schema is introspectable; the site configuration schema is at \
         `{base}/.api/site-config-schema`. Details: {DOCS_URL}"
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "https://sg.example.com";

    #[test]
    fn database_route() {
        let text = instructions(BASE, &Sources::default());
        assert!(text.contains("https://sg.example.com/.api/graphql"));
        assert!(text.contains("SOURCEGULL_ADMIN_TOKEN"));
        assert!(text.contains("updateSiteConfiguration"));
        assert!(text.contains("overwriteSettings"));
        assert!(!text.contains("from files"));
    }

    #[test]
    fn file_route() {
        let s = Sources {
            site_config_file: Some("/etc/sourcegull/site.jsonc".into()),
            global_settings_file: Some("/etc/sourcegull/settings.jsonc".into()),
            resources_file: Some((
                "/etc/sourcegull/resources.jsonc".into(),
                vec!["roles".into()],
            )),
        };
        let text = instructions(BASE, &s);
        assert!(text.contains("SOURCEGULL_SITE_CONFIG_FILE, `/etc/sourcegull/site.jsonc`"));
        assert!(text.contains("SOURCEGULL_GLOBAL_SETTINGS_FILE"));
        assert!(text.contains("resources.jsonc`: roles"));
        assert!(text.contains("sourcegull config check"));
        assert!(!text.contains("updateSiteConfiguration"));
        assert!(!text.contains("overwriteSettings"));
    }

    #[test]
    fn site_config_file_only() {
        let s = Sources {
            site_config_file: Some("/site.jsonc".into()),
            ..Default::default()
        };
        let text = instructions(BASE, &s);
        assert!(!text.contains("updateSiteConfiguration"));
        assert!(text.contains("overwriteSettings"));
    }
}
