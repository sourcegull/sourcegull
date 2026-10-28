//! Site config sections: every key has one section, and the default document is
//! ordered by section.

use std::collections::HashSet;

use sourcegull_config::*;

#[test]
fn every_schema_property_is_in_exactly_one_section() {
    let schema = site_config_schema();
    let props = schema["properties"].as_object().unwrap();
    let mut listed = HashSet::new();
    for s in SITE_CONFIG_SECTIONS {
        for key in s.keys {
            assert!(listed.insert(*key), "{key} is in two sections");
            assert!(
                props.contains_key(*key),
                "section {} lists unknown key {key}",
                s.id
            );
        }
    }
    for (key, prop) in props {
        assert!(listed.contains(key.as_str()), "{key} has no section");
        assert_eq!(prop["x-section"], section_for_key(key).id, "{key}");
    }
    let ids: HashSet<_> = SITE_CONFIG_SECTIONS.iter().map(|s| s.id).collect();
    assert_eq!(
        ids.len(),
        SITE_CONFIG_SECTIONS.len(),
        "section ids are unique"
    );
    assert!(!ids.contains(OTHER_SECTION.id));
}

#[test]
fn schema_lists_sections_in_order() {
    let schema = site_config_schema();
    let sections = schema["x-sections"].as_array().unwrap();
    let ids: Vec<_> = sections.iter().map(|s| s["id"].as_str().unwrap()).collect();
    let expected: Vec<_> = SITE_CONFIG_SECTIONS.iter().map(|s| s.id).collect();
    assert_eq!(ids, expected);
    assert_eq!(sections[1]["title"], "Authentication & sign-in");
    assert!(
        sections[1]["keys"]
            .as_array()
            .unwrap()
            .contains(&"auth.providers".into())
    );
    // The annotations do not affect validation.
    assert!(
        validate(r#"{"auth.public": true}"#)
            .unwrap()
            .warnings
            .is_empty()
    );
}

#[test]
fn unknown_keys_go_to_other() {
    assert_eq!(section_for_key("nope").id, "other");
    assert_eq!(section_for_key("search.limits").id, "search");
}

#[test]
fn default_config_is_organized() {
    let text = default_site_config_jsonc();
    assert_eq!(
        organize_site_config(text).unwrap(),
        text,
        "the default config is in section order with headers"
    );
    for s in SITE_CONFIG_SECTIONS {
        assert!(
            text.contains(&section_header(s)),
            "the default config has a header for {}",
            s.id
        );
    }
}

#[test]
fn organize_keeps_the_value() {
    let messy = r#"{
  "outboundRequestLogLimit": 0,
  "batchChanges.enabled": false, // off for now
  "log": { "securityEventLog": { "location": "database" } },
  // Who can sign in.
  "auth.providers": [{ "type": "builtin", "allowSignup": true }],
  "externalURL": "https://sourcegull.example.com",
}"#;
    let out = organize_site_config(messy).unwrap();
    assert_eq!(parse_jsonc(&out).unwrap(), parse_jsonc(messy).unwrap());
    let pos = |needle: &str| {
        out.find(needle)
            .unwrap_or_else(|| panic!("{needle} in {out}"))
    };
    assert!(pos("\"externalURL\"") < pos("// Who can sign in."));
    assert!(pos("// Who can sign in.") < pos("\"auth.providers\""));
    assert!(pos("\"auth.providers\"") < pos("\"batchChanges.enabled\": false, // off for now"));
    assert!(pos("\"batchChanges.enabled\"") < pos("\"log\""));
    assert!(pos("\"log\"") < pos("\"outboundRequestLogLimit\""));
    assert!(pos("// ── Bulk changes ──") < pos("\"batchChanges.enabled\""));
}

/// Instances created before sections were added store the previous default document.
#[test]
fn organizes_the_previous_default_config() {
    let old = include_str!("fixtures/previous_default_site_config.jsonc");
    let out = organize_site_config(old).unwrap();
    assert_eq!(parse_jsonc(&out).unwrap(), parse_jsonc(old).unwrap());
    assert_eq!(organize_site_config(&out).unwrap(), out, "idempotent");
    let pos = |needle: &str| {
        out.find(needle)
            .unwrap_or_else(|| panic!("{needle} in {out}"))
    };
    // Comments stay with their keys, commented-out keys go to their section.
    assert!(pos("// ── General ──") < pos("// \"externalURL\""));
    assert!(pos("// \"externalURL\"") < pos("// ── Authentication & sign-in ──"));
    assert!(pos("// Allow signed-out visitors to search.") + 45 > pos("\"auth.public\": false"));
    assert!(pos("// ── Users & permissions ──") < pos("// \"permissions.userMapping\""));
    assert!(pos("// ── Search & indexing ──") < pos("// \"search.index.branches\""));
    assert!(pos("// ── Observability & logs ──") < pos("\"log\""));
    // Keys this version no longer knows go to "Other" at the end.
    assert!(pos("\"log\"") < pos("// ── Other ──"));
    assert!(pos("// ── Other ──") < pos("// \"observability.tracing\""));
    assert!(pos("// ── Other ──") < pos("// \"experimentalFeatures\""));
    assert!(
        out.ends_with("  \"experimentalFeatures\": {},\n}\n"),
        "{out}"
    );
}

#[test]
fn every_settings_property_is_in_a_section() {
    let schema = settings_schema();
    let props = schema["properties"].as_object().unwrap();
    for (key, prop) in props {
        let section = find_section(SETTINGS_SECTIONS, key);
        assert_ne!(
            section.id, OTHER_SECTION.id,
            "{key} has no settings section"
        );
        assert_eq!(prop["x-section"], section.id);
    }
    for s in SETTINGS_SECTIONS {
        for key in s.keys {
            assert!(props.contains_key(*key), "unknown settings key {key}");
        }
    }
}

#[test]
fn default_settings_list_every_setting_commented_out() {
    let text = default_settings_jsonc();
    assert_eq!(parse_jsonc(&text).unwrap(), serde_json::json!({}));
    assert_eq!(
        organize_document(&text, SETTINGS_SECTIONS).unwrap(),
        text,
        "in section order"
    );
    for key in settings_schema()["properties"].as_object().unwrap().keys() {
        assert!(
            text.contains(&format!("  // \"{key}\": ")),
            "{key} in {text}"
        );
    }
    assert!(text.contains("  // ── Search ──\n"));
    assert!(
        text.contains("  // \"search.defaultPatternType\": \"keyword\",\n"),
        "{text}"
    );
    assert!(text.contains("\"keyword\""), "enum values are listed");
    assert!(text.contains("  // \"quicklinks\": [],\n"));
}

#[test]
fn commented_reference_of_site_config_is_organized() {
    let text = commented_reference(&site_config_schema(), SITE_CONFIG_SECTIONS);
    assert_eq!(parse_jsonc(&text).unwrap(), serde_json::json!({}));
    assert_eq!(organize_site_config(&text).unwrap(), text);
    assert!(text.contains("  // \"auth.public\": false,\n"));
}
