use std::time::Duration;

use serde_json::json;
use sourcegull_config::*;

fn invalid(text: &str) -> Vec<ConfigIssue> {
    match validate(text) {
        Err(ConfigError::Invalid(issues)) => issues,
        other => panic!("expected Invalid, got {other:?}"),
    }
}

#[test]
fn empty_config_gets_defaults() {
    let v = validate("{}").unwrap();
    assert!(v.warnings.is_empty());
    let c = v.config;
    assert_eq!(c, SiteConfig::default());
    assert_eq!(
        c.auth_providers,
        vec![AuthProvider::Builtin(BuiltinAuthProvider {
            allow_signup: false
        })]
    );
    assert_eq!(c.auth_session_expiry.get(), Duration::from_secs(720 * 3600));
    assert_eq!(c.auth_lockout.failed_attempt_threshold, 5);
    assert_eq!(c.auth_access_tokens.allow, AccessTokenAllow::All);
    assert_eq!(c.auth_access_request.enabled, None);
    assert_eq!(
        c.log.security_event_log.location,
        SecurityEventLogLocation::Database
    );
    assert_eq!(c.search_limits.default_timeout_seconds, 20);
    assert_eq!(c.search_limits.max_timeout_seconds, 60);
    assert_eq!(c.search_limits.streaming_max_results, 500);
    assert_eq!(c.search_limits.commit_diff_max_repos, 50);
    assert!(c.pinned_repos().is_empty());
}

#[test]
fn header_provider_and_access_request_config() {
    let c = validate(
        r#"{
        "auth.providers": [{"type":"http-header", "usernameHeader":"X-User",
            "emailHeader":"X-Email", "stripUsernameHeaderPrefix":"accounts.google.com:",
            "trustedProxies":["10.0.0.0/8"]}],
        "auth.accessRequest":{"enabled":false}
    }"#,
    )
    .unwrap()
    .config;
    assert_eq!(c.auth_access_request.enabled, Some(false));
    let AuthProvider::HttpHeader(header) = &c.auth_providers[0] else {
        panic!("header provider")
    };
    assert_eq!(
        header.strip_username_header_prefix.as_deref(),
        Some("accounts.google.com:")
    );
    assert_eq!(header.trusted_proxies, ["10.0.0.0/8"]);
}

#[test]
fn github_and_gitea_providers() {
    let c = validate(
        r#"{"auth.providers": [
          {"type": "github", "clientID": "gh", "clientSecret": "s", "allowOrgs": ["acme"]},
          {"type": "gitea", "url": "https://gitea.example.com", "clientID": "gt", "clientSecret": "s"}
        ]}"#,
    )
    .unwrap()
    .config;
    match &c.auth_providers[..] {
        [AuthProvider::Github(gh), AuthProvider::Gitea(gt)] => {
            assert_eq!(gh.url, "https://github.com/");
            assert_eq!(gh.allow_orgs, vec!["acme"]);
            assert!(!gh.allow_signup);
            assert_eq!(gt.url, "https://gitea.example.com");
            assert!(gt.allow_orgs.is_empty());
        }
        other => panic!("{other:?}"),
    }
    // Gitea has no default instance.
    assert!(
        !invalid(
            r#"{"auth.providers": [{"type": "gitea", "clientID": "a", "clientSecret": "b"}]}"#
        )
        .is_empty()
    );
}

#[test]
fn saml_providers_are_rejected() {
    let text = r#"{"auth.providers": [
        {"type": "builtin"},
        {"type": "saml", "identityProviderMetadataURL": "https://idp/meta"}
    ]}"#;
    let issues = invalid(text);
    assert_eq!(issues.len(), 1, "{issues:?}");
    assert_eq!(issues[0].pointer, "/auth.providers/1");
    assert!(
        issues[0]
            .message
            .starts_with("SAML sign-in is not supported"),
        "{}",
        issues[0].message
    );
    // Other errors are still reported next to it.
    let issues = invalid(r#"{"externalURL": 42, "auth.providers": [{"type": "saml"}]}"#);
    assert_eq!(
        issues
            .iter()
            .map(|i| i.pointer.as_str())
            .collect::<Vec<_>>(),
        ["/auth.providers/0", "/externalURL"]
    );

    // The loader removes them instead and keeps the rest.
    let mut value = parse_jsonc(text).unwrap();
    let removed = remove_unsupported_auth_providers(&mut value);
    assert_eq!(removed.len(), 1);
    assert_eq!(removed[0].pointer, "/auth.providers/1");
    let c = validate_value(&value).unwrap().config;
    assert!(matches!(
        c.auth_providers.as_slice(),
        [AuthProvider::Builtin(_)]
    ));
}

#[test]
fn default_jsonc_is_valid_and_matches_defaults() {
    let v = validate(default_site_config_jsonc()).unwrap();
    assert!(v.warnings.is_empty(), "{:?}", v.warnings);
    assert_eq!(v.config, SiteConfig::default());
}

#[test]
fn full_config_round_trips() {
    let text = r#"{
      "externalURL": "https://sourcegull.example.com",
      "auth.providers": [
        {"type": "builtin", "allowSignup": true},
        {"type": "gitlab", "clientID": "id", "clientSecret": "s", "allowGroups": ["eng"]},
        {"type": "openidconnect", "issuer": "https://idp", "clientID": "c", "clientSecret": "s"},
        {"type": "http-header", "usernameHeader": "X-User", "emailHeader": "X-Email"},
      ],
      "auth.sessionExpiry": "90d",
      "auth.passwordPolicy": {"enabled": true, "minimumLength": 14, "numberOfSpecialCharacters": 1,
                             "requireAtLeastOneNumber": true, "requireUpperandLowerCase": true},
      "auth.accessTokens": {"allow": "site-admin-create", "maxTokenExpiresInDays": 30},
      "auth.public": true,
      "log": {"securityEventLog": {"location": "all"}},
      "search.limits": {"maxRepos": 500, "defaultTimeoutSeconds": 10},
      "repoNodes.pinnedRepos": {"gitlab.com/acme/big": "http://sourcegull-repo-2:3178"},
    }"#;
    let v = validate(text).unwrap();
    assert!(v.warnings.is_empty(), "{:?}", v.warnings);
    let c = v.config;
    assert_eq!(
        c.external_url.as_deref(),
        Some("https://sourcegull.example.com")
    );
    assert_eq!(c.auth_providers.len(), 4);
    match &c.auth_providers[1] {
        AuthProvider::Gitlab(g) => {
            assert_eq!(g.url, "https://gitlab.com/");
            assert_eq!(g.client_id, "id");
            assert_eq!(g.allow_groups, vec!["eng"]);
            assert!(!g.allow_signup);
        }
        other => panic!("{other:?}"),
    }
    assert!(
        matches!(&c.auth_providers[3], AuthProvider::HttpHeader(h) if h.username_header == "X-User")
    );
    assert_eq!(
        c.auth_session_expiry.get(),
        Duration::from_secs(90 * 86_400)
    );
    assert!(c.auth_password_policy.require_upperand_lower_case);
    assert_eq!(
        c.auth_access_tokens.allow,
        AccessTokenAllow::SiteAdminCreate
    );
    assert_eq!(c.search_limits.max_repos, Some(500));
    assert_eq!(c.search_limits.default_timeout_seconds, 10);
    assert_eq!(c.search_limits.max_timeout_seconds, 60);
    assert_eq!(
        c.pinned_repos(),
        [(
            "gitlab.com/acme/big".to_owned(),
            "http://sourcegull-repo-2:3178".to_owned()
        )]
        .into()
    );

    // Serializing and re-validating yields the same config.
    let again = validate_value(&serde_json::to_value(&c).unwrap()).unwrap();
    assert_eq!(again.config, c);
}

#[test]
fn unknown_keys_are_warnings_with_paths() {
    let text = r#"{
      "someFutureKey": 1,
      "auth.lockout": {"lockoutPeriod": 60, "typo": true},
      "auth.providers": [{"type": "gitlab", "clientID": "a", "clientSecret": "b", "extra": 1}],
      "redis.url": "redis://redis:6379",
      "observability.tracing": {"sampling": "all"},
    }"#;
    let v = validate(text).unwrap();
    let mut pointers: Vec<_> = v.warnings.iter().map(|w| w.pointer.as_str()).collect();
    pointers.sort();
    assert_eq!(
        pointers,
        vec![
            "/auth.lockout/typo",
            "/auth.providers/0/extra",
            "/observability.tracing",
            "/redis.url",
            "/someFutureKey"
        ]
    );
    assert_eq!(v.config.auth_lockout.lockout_period, 60);
}

fn warnings(text: &str) -> Vec<(String, String)> {
    let mut w: Vec<_> = validate(text)
        .unwrap()
        .warnings
        .into_iter()
        .map(|w| (w.pointer, w.message))
        .collect();
    w.sort();
    w
}

#[test]
fn legacy_pinned_repos_are_read_with_a_deprecation_warning() {
    let text = r#"{
      "experimentalFeatures": {
        "gitServerPinnedRepos": {"gitlab.com/a/big": "http://repo-2:3178", "gitlab.com/a/bad": 3},
        "structuralSearch": "enabled",
      },
    }"#;
    let c = validate(text).unwrap().config;
    assert_eq!(
        c.pinned_repos(),
        [(
            "gitlab.com/a/big".to_owned(),
            "http://repo-2:3178".to_owned()
        )]
        .into()
    );
    let w = warnings(text);
    let pointers: Vec<_> = w.iter().map(|(p, _)| p.as_str()).collect();
    assert_eq!(
        pointers,
        vec![
            "/experimentalFeatures/gitServerPinnedRepos",
            "/experimentalFeatures/gitServerPinnedRepos/gitlab.com~1a~1bad",
            "/experimentalFeatures/structuralSearch",
        ]
    );
    assert!(w[0].1.contains("repoNodes.pinnedRepos"), "{w:?}");
    assert!(w[1].1.contains("must be a string"), "{w:?}");
    assert!(w[2].1.contains("unknown property"), "{w:?}");

    // An empty object from an older default config is only reported as deprecated.
    let w = warnings(r#"{"experimentalFeatures": {}}"#);
    assert_eq!(w.len(), 1, "{w:?}");
    assert_eq!(w[0].0, "/experimentalFeatures");
    assert!(w[0].1.contains("deprecated"), "{w:?}");
}

#[test]
fn new_pinned_repos_key_wins_over_the_legacy_one() {
    let text = r#"{
      "repoNodes.pinnedRepos": {"gitlab.com/a/x": "http://repo-1:3178"},
      "experimentalFeatures": {"gitServerPinnedRepos": {"gitlab.com/a/y": "http://repo-2:3178"}},
    }"#;
    let c = validate(text).unwrap().config;
    assert_eq!(
        c.pinned_repos(),
        [("gitlab.com/a/x".to_owned(), "http://repo-1:3178".to_owned())].into()
    );
    let w = warnings(text);
    assert_eq!(w.len(), 1, "{w:?}");
    assert!(w[0].1.contains("ignored"), "{w:?}");
}

#[test]
fn pinned_repo_values_must_be_strings() {
    let issues = invalid(r#"{"repoNodes.pinnedRepos": {"gitlab.com/a/x": 1}}"#);
    assert_eq!(issues[0].pointer, "/repoNodes.pinnedRepos/gitlab.com~1a~1x");
}

#[test]
fn type_errors_carry_json_pointers() {
    let issues = invalid(r#"{"auth.public": "yes", "search.limits": {"maxTimeoutSeconds": -1}}"#);
    let pointers: Vec<_> = issues.iter().map(|i| i.pointer.as_str()).collect();
    assert_eq!(
        pointers,
        vec!["/auth.public", "/search.limits/maxTimeoutSeconds"]
    );
}

#[test]
fn bad_enum_and_duration() {
    let issues =
        invalid(r#"{"auth.accessTokens": {"allow": "everyone"}, "auth.sessionExpiry": "90 days"}"#);
    let pointers: Vec<_> = issues.iter().map(|i| i.pointer.as_str()).collect();
    assert_eq!(
        pointers,
        vec!["/auth.accessTokens/allow", "/auth.sessionExpiry"]
    );
}

#[test]
fn provider_missing_required_field() {
    let issues = invalid(
        r#"{"auth.providers": [{"type": "builtin"}, {"type": "gitlab", "clientID": "x"}]}"#,
    );
    assert_eq!(issues.len(), 1, "{issues:?}");
    assert_eq!(issues[0].pointer, "/auth.providers/1");
    let issues = invalid(r#"{"auth.providers": [{"type": "kerberos"}]}"#);
    assert_eq!(issues[0].pointer, "/auth.providers/0");
}

#[test]
fn syntax_errors_have_positions() {
    match validate("{\n  // ok\n  \"auth.public\": true\n  \"x\": 1\n}") {
        Err(ConfigError::Syntax(e)) => assert_eq!(e.line, 4),
        other => panic!("{other:?}"),
    }
}

#[test]
fn root_must_be_object() {
    let issues = invalid("[1, 2]");
    assert_eq!(issues[0].pointer, "");
}

#[test]
fn schema_shape() {
    let schema = site_config_schema();
    let props = &schema["properties"];
    for key in [
        "externalURL",
        "auth.providers",
        "auth.sessionExpiry",
        "auth.passwordPolicy",
        "auth.lockout",
        "auth.accessTokens",
        "auth.public",
        "log",
        "search.limits",
        "repoNodes.pinnedRepos",
    ] {
        assert!(props.get(key).is_some(), "missing {key}");
    }
    assert_eq!(props["auth.public"]["default"], json!(false));
}

#[test]
fn schema_marks_secrets() {
    let schema = site_config_schema();
    let props = &schema["properties"];
    assert_eq!(props["executors.accessToken"]["x-secret"], json!(true));
    assert_eq!(props["scim.authToken"]["x-secret"], json!(true));
    assert!(props["externalURL"].get("x-secret").is_none());
    let defs = &schema["definitions"];
    assert_eq!(
        defs["SmtpConfig"]["properties"]["password"]["x-secret"],
        json!(true)
    );
    assert!(
        defs["SmtpConfig"]["properties"]["username"]
            .get("x-secret")
            .is_none()
    );
    let providers = defs["AuthProvider"]["oneOf"].as_array().unwrap();
    let gitlab = providers
        .iter()
        .find(|p| p["properties"]["type"]["enum"][0] == "gitlab")
        .unwrap();
    assert_eq!(
        gitlab["properties"]["clientSecret"]["x-secret"],
        json!(true)
    );
    assert!(gitlab["properties"]["clientID"].get("x-secret").is_none());
}

#[test]
fn rollout_windows() {
    let v = validate(
        r#"{"batchChanges.rolloutWindows": [
            {"rate": "10/hour"},
            {"days": ["saturday", "sunday"], "start": "08:00", "end": "20:00", "rate": 0}
        ]}"#,
    )
    .unwrap();
    assert!(v.warnings.is_empty(), "{:?}", v.warnings);
    let windows = RolloutWindows::parse(&v.config.batch_changes_rollout_windows).unwrap();
    assert_eq!(windows.windows.len(), 2);
    assert_eq!(windows.windows[1].rate, RolloutRate::Paused);

    // Semantic errors point into the list.
    let issues = invalid(
        r#"{"batchChanges.rolloutWindows": [{"rate": "unlimited"},
            {"start": "18:00", "end": "08:00", "rate": "1/minute"}]}"#,
    );
    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0].pointer, "/batchChanges.rolloutWindows/1/end");
    let issues = invalid(r#"{"batchChanges.rolloutWindows": [{"rate": "lots"}]}"#);
    assert_eq!(issues[0].pointer, "/batchChanges.rolloutWindows/0/rate");
    // Schema errors: unknown day, missing rate.
    let issues = invalid(r#"{"batchChanges.rolloutWindows": [{"days": ["funday"], "rate": 0}]}"#);
    assert!(
        issues[0]
            .pointer
            .starts_with("/batchChanges.rolloutWindows/0")
    );
    let issues = invalid(r#"{"batchChanges.rolloutWindows": [{"days": ["monday"]}]}"#);
    assert!(
        issues[0]
            .pointer
            .starts_with("/batchChanges.rolloutWindows/0")
    );
    // Unknown keys in a window are warnings.
    let v = validate(r#"{"batchChanges.rolloutWindows": [{"rate": 0, "speed": 1}]}"#).unwrap();
    assert_eq!(
        v.warnings[0].pointer,
        "/batchChanges.rolloutWindows/0/speed"
    );
}

#[test]
fn always_index_patterns_combine_and_are_validated() {
    let c = validate(
        r#"{"search.index.alwaysIndex": {"*": ["**/*.graphql"], "gitlab.com/a/x": ["gen/*"]}}"#,
    )
    .unwrap()
    .config;
    let x = c.always_index("gitlab.com/a/x");
    assert_eq!(x.patterns(), ["**/*.graphql", "gen/*"]);
    assert!(x.matches("gen/api.ts") && x.matches("a/schema.graphql"));
    let y = c.always_index("gitlab.com/a/y");
    assert!(!y.matches("gen/api.ts") && y.matches("schema.graphql"));
    assert!(
        SiteConfig::default()
            .always_index("gitlab.com/a/x")
            .is_empty()
    );

    let issues = invalid(r#"{"search.index.alwaysIndex": {"gitlab.com/a/x": ["ok", "gen/[a"]}}"#);
    assert_eq!(issues.len(), 1, "{issues:?}");
    assert_eq!(
        issues[0].pointer,
        "/search.index.alwaysIndex/gitlab.com~1a~1x/1"
    );
}
