//! Sections of the site configuration and of settings: every key belongs to one
//! section, which orders the document, its section headers (`// ── Search & indexing ──`)
//! and the section navigation of the editors. The schemas carry them as `x-sections` (at the
//! root) and `x-section` (on each property).

/// A group of related site configuration (or settings) keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfigSection {
    /// Stable identifier, e.g. `"auth"`.
    pub id: &'static str,
    /// Heading shown in the editor and in the document's section headers.
    pub title: &'static str,
    /// One-line description.
    pub description: &'static str,
    /// The section's keys, in the order they appear in the document.
    pub keys: &'static [&'static str],
}

/// The section for keys the schema does not know (typos, keys of other versions).
pub const OTHER_SECTION: ConfigSection = ConfigSection {
    id: "other",
    title: "Other",
    description: "Keys this version does not know.",
    keys: &[],
};

/// Every site configuration section, in document order. Each schema property is in exactly
/// one of them.
pub const SITE_CONFIG_SECTIONS: &[ConfigSection] = &[
    ConfigSection {
        id: "general",
        title: "General",
        description: "The URL users reach this instance at.",
        keys: &["externalURL"],
    },
    ConfigSection {
        id: "auth",
        title: "Authentication & sign-in",
        description: "Sign-in providers, sessions, passwords, lockout, rate limits, access tokens, MCP sign-in and IP restrictions.",
        keys: &[
            "auth.providers",
            "auth.public",
            "auth.enableUsernameChanges",
            "auth.sessionExpiry",
            "auth.passwordPolicy",
            "auth.passwordResetLinkExpiry",
            "auth.lockout",
            "auth.rateLimits",
            "auth.accessRequest",
            "auth.accessTokens",
            "mcp.oauth.enabled",
            "mcp.oauth.dynamicClientRegistration",
            "mcp.oauth.accessTokenTTL",
            "mcp.oauth.refreshTokenTTL",
            "mcp.oauth.allowedRedirectHosts",
            "auth.allowedIpAddress",
        ],
    },
    ConfigSection {
        id: "permissions",
        title: "Users & permissions",
        description: "Repository permissions and their sync, SCIM provisioning and teams.",
        keys: &[
            "permissions.userMapping",
            "authz.enforceForSiteAdmins",
            "permissions.syncScheduleInterval",
            "permissions.syncOldestUsers",
            "permissions.syncOldestRepos",
            "permissions.syncUsersBackoffSeconds",
            "permissions.syncReposBackoffSeconds",
            "permissions.syncJobsHistorySize",
            "scim.authToken",
            "own.syncTeamsFromGitLabGroups",
        ],
    },
    ConfigSection {
        id: "repositories",
        title: "Repositories & git",
        description: "Code host discovery syncs, repository fetches, clones and pinning on the repo nodes.",
        keys: &[
            "repoListUpdateInterval",
            "disableAutoGitUpdates",
            "gitUpdateInterval",
            "gitMaxConcurrentClones",
            "repoNodes.pinnedRepos",
        ],
    },
    ConfigSection {
        id: "search",
        title: "Search & indexing",
        description: "Search timeouts, indexed branches and files, and search job retention.",
        keys: &[
            "search.limits",
            "search.index.branches",
            "search.index.alwaysIndex",
            "searchJobs.retention",
        ],
    },
    ConfigSection {
        id: "codeIntel",
        title: "Code intelligence & executors",
        description: "SCIP uploads, ranking, auto-indexing and the executor access token.",
        keys: &[
            "executors.accessToken",
            "codeIntel.gitlabJobTokenUploads",
            "codeIntel.maxUploadBytes",
            "codeIntelRanking.documentReferenceCountsEnabled",
            "codeIntelAutoIndexing.enabled",
            "codeIntelAutoIndexing.allowGlobalPolicies",
            "codeIntelAutoIndexing.policyRepositoryMatchLimit",
            "codeIntelAutoIndexing.indexerMap",
            "codeIntelAutoIndexing.repositoryCommitsPerHour",
        ],
    },
    ConfigSection {
        id: "batchChanges",
        title: "Bulk changes",
        description: "Who may use bulk changes, how changesets are pushed and published, retention.",
        keys: &[
            "batchChanges.enabled",
            "batchChanges.restrictToAdmins",
            "batchChanges.enforceForks",
            "batchChanges.autoDeleteBranch",
            "batchChanges.commitViaAPI",
            "batchChanges.rejectUnverifiedCommit",
            "batchChanges.commitViaAPIMaxBytes",
            "batchChanges.commitViaAPIMaxFiles",
            "batchChanges.rolloutWindows",
            "batchChanges.reconcilerConcurrency",
            "batchChanges.changesetsRetention",
        ],
    },
    ConfigSection {
        id: "insights",
        title: "Trends",
        description: "Backfill rate, concurrency and how many points a series keeps.",
        keys: &[
            "insights.query.worker.rateLimit",
            "insights.query.worker.concurrency",
            "insights.maximumSampleSize",
        ],
    },
    ConfigSection {
        id: "notifications",
        title: "Email & webhooks",
        description: "Outbound email (SMTP), outbound webhook deliveries and watch results.",
        keys: &[
            "email.address",
            "email.senderName",
            "email.smtp",
            "webhooks.outbound.allowedHosts",
            "webhooks.outbound.logRetention",
            "watches.allowUnfilteredResults",
        ],
    },
    ConfigSection {
        id: "observability",
        title: "Observability & logs",
        description: "Logs, slow request logs and usage event retention.",
        keys: &[
            "log",
            "observability.logSlowGraphQLRequests",
            "observability.logSlowSearches",
            "outboundRequestLogLimit",
            "eventLogs.retention",
            "eventLogs.rollupRetention",
        ],
    },
];

/// The section `key` belongs to, or [`OTHER_SECTION`] for a key no section lists.
pub fn section_for_key(key: &str) -> &'static ConfigSection {
    find_section(SITE_CONFIG_SECTIONS, key)
}

/// The section of `sections` that lists `key`, or [`OTHER_SECTION`].
pub fn find_section<'a>(sections: &'a [ConfigSection], key: &str) -> &'a ConfigSection {
    sections
        .iter()
        .find(|s| s.keys.contains(&key))
        .unwrap_or(&OTHER_SECTION)
}

/// `(section index, key index)` of `key` in `sections`, for sorting; unknown keys sort last.
pub(crate) fn key_rank(sections: &[ConfigSection], key: &str) -> (usize, usize) {
    sections
        .iter()
        .enumerate()
        .find_map(|(si, s)| s.keys.iter().position(|k| *k == key).map(|ki| (si, ki)))
        .unwrap_or((sections.len(), 0))
}

/// The header line of a section in the document, e.g. `// ── Search & indexing ──`.
pub fn section_header(section: &ConfigSection) -> String {
    format!("// ── {} ──", section.title)
}

/// Whether `line` (trimmed) is a section header written by [`section_header`].
pub(crate) fn is_section_header(line: &str) -> bool {
    let t = line.trim();
    t.starts_with("// ── ") && t.ends_with(" ──")
}

/// Adds `x-sections` to the schema root and `x-section` to each property.
pub(crate) fn annotate_schema(schema: &mut serde_json::Value, sections: &[ConfigSection]) {
    use serde_json::{Value, json};
    if let Some(props) = schema.get_mut("properties").and_then(Value::as_object_mut) {
        for (key, prop) in props.iter_mut() {
            if let Some(obj) = prop.as_object_mut() {
                obj.insert("x-section".into(), json!(find_section(sections, key).id));
            }
        }
    }
    let sections: Vec<Value> = sections
        .iter()
        .map(|s| {
            json!({ "id": s.id, "title": s.title, "description": s.description, "keys": s.keys })
        })
        .collect();
    if let Some(root) = schema.as_object_mut() {
        root.insert("x-sections".into(), Value::Array(sections));
    }
}

/// Every settings section (user, org and global settings), in document order.
pub const SETTINGS_SECTIONS: &[ConfigSection] = &[
    ConfigSection {
        id: "search",
        title: "Search",
        description: "Defaults for search queries.",
        keys: &[
            "search.defaultPatternType",
            "search.includeForks",
            "search.includeArchived",
        ],
    },
    ConfigSection {
        id: "home",
        title: "Home page & notices",
        description: "Links on the search home page and banners shown to users.",
        keys: &["quicklinks", "notices"],
    },
];

/// A JSONC document listing every property of `schema`, commented out with its default and
/// a one-line description, grouped under the headers of `sections`.
pub fn commented_reference(schema: &serde_json::Value, sections: &[ConfigSection]) -> String {
    use serde_json::Value;
    let empty = serde_json::Map::new();
    let props = schema
        .get("properties")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    let mut keys: Vec<&String> = props.keys().collect();
    keys.sort_by_key(|k| key_rank(sections, k));

    let mut out = String::from("{\n");
    let mut current = None;
    for key in keys {
        let prop = resolve(schema, &props[key.as_str()]);
        let section = key_rank(sections, key).0;
        if current != Some(section) {
            if current.is_some() {
                out.push('\n');
            }
            let s = sections.get(section).unwrap_or(&OTHER_SECTION);
            out.push_str(&format!("  {}\n", section_header(s)));
            current = Some(section);
        } else {
            out.push('\n');
        }
        let mut description = one_line(
            prop.get("description")
                .and_then(Value::as_str)
                .unwrap_or(""),
        );
        let values = enum_values(schema, &prop);
        if !values.is_empty() {
            if !description.is_empty() {
                description.push(' ');
            }
            description.push_str(&format!("One of {}.", values.join(", ")));
        }
        for line in wrap(&description, 94) {
            out.push_str(&format!("  // {line}\n"));
        }
        let example = prop
            .get("default")
            .cloned()
            .unwrap_or_else(|| placeholder(&prop));
        let key_json = serde_json::to_string(key).unwrap_or_default();
        let value_json = serde_json::to_string(&example).unwrap_or_default();
        out.push_str(&format!("  // {key_json}: {value_json},\n"));
    }
    out.push_str("}\n");
    out
}

/// Follows `$ref` (also wrapped in a one-element `allOf`), keeping the property's own keys.
fn resolve(root: &serde_json::Value, prop: &serde_json::Value) -> serde_json::Value {
    let mut node = prop.clone();
    for _ in 0..10 {
        let reference = node
            .get("$ref")
            .or_else(|| node.pointer("/allOf/0/$ref"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        let Some(reference) = reference else { break };
        let Some(target) = reference
            .strip_prefix('#')
            .and_then(|p| root.pointer(p))
            .and_then(serde_json::Value::as_object)
        else {
            break;
        };
        let mut merged = target.clone();
        if let Some(own) = node.as_object() {
            for (k, v) in own {
                if k != "$ref" && k != "allOf" {
                    merged.insert(k.clone(), v.clone());
                }
            }
        }
        node = serde_json::Value::Object(merged);
    }
    node
}

/// The allowed values of an enumeration, as JSON.
fn enum_values(root: &serde_json::Value, prop: &serde_json::Value) -> Vec<String> {
    let direct = prop.get("enum").and_then(serde_json::Value::as_array);
    let variants = prop
        .get("oneOf")
        .or_else(|| prop.get("anyOf"))
        .and_then(serde_json::Value::as_array);
    let mut out: Vec<String> = direct
        .into_iter()
        .flatten()
        .map(ToString::to_string)
        .collect();
    for v in variants.into_iter().flatten() {
        let v = resolve(root, v);
        let values = v.get("enum").and_then(serde_json::Value::as_array);
        out.extend(values.into_iter().flatten().map(ToString::to_string));
    }
    out
}

/// An example value for a property without a default.
fn placeholder(prop: &serde_json::Value) -> serde_json::Value {
    let ty = match prop.get("type") {
        Some(serde_json::Value::Array(types)) => types
            .iter()
            .filter_map(serde_json::Value::as_str)
            .find(|t| *t != "null")
            .unwrap_or("null")
            .to_owned(),
        Some(serde_json::Value::String(t)) => t.clone(),
        _ => String::new(),
    };
    match ty.as_str() {
        "array" => serde_json::json!([]),
        "object" => serde_json::json!({}),
        "string" => serde_json::json!(""),
        "boolean" => serde_json::json!(false),
        "integer" | "number" => serde_json::json!(0),
        _ => serde_json::Value::Null,
    }
}

/// The first paragraph of a doc comment, on one line.
fn one_line(description: &str) -> String {
    let paragraph = description.split("\n\n").next().unwrap_or("");
    paragraph.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Splits `text` into lines of at most `width` characters (longer words stay whole).
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for word in text.split_whitespace() {
        match lines.last_mut() {
            Some(line) if line.chars().count() + 1 + word.chars().count() <= width => {
                line.push(' ');
                line.push_str(word);
            }
            _ => lines.push(word.to_owned()),
        }
    }
    lines
}
