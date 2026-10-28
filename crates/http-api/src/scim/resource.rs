//! SCIM User resources (RFC 7643 §4.1): rendering users as resources, reading the desired
//! state out of a resource an identity provider sent, attribute projection, and the discovery
//! documents (`ServiceProviderConfig`, `ResourceTypes`, `Schemas`).

use chrono::SecondsFormat;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use sourcegull_db::{ScimUser, ScimWrite};

use super::ScimError;
use super::filter::{AttrPath, get_ci};

/// The core User schema.
pub const CORE_USER_SCHEMA: &str = "urn:ietf:params:scim:schemas:core:2.0:User";
/// The enterprise User extension.
pub const ENTERPRISE_USER_SCHEMA: &str =
    "urn:ietf:params:scim:schemas:extension:enterprise:2.0:User";
/// List responses.
pub const LIST_RESPONSE_SCHEMA: &str = "urn:ietf:params:scim:api:messages:2.0:ListResponse";
/// PATCH requests.
pub const PATCH_OP_SCHEMA: &str = "urn:ietf:params:scim:api:messages:2.0:PatchOp";
/// Error responses.
pub const ERROR_SCHEMA: &str = "urn:ietf:params:scim:api:messages:2.0:Error";

const SERVICE_PROVIDER_CONFIG_SCHEMA: &str =
    "urn:ietf:params:scim:schemas:core:2.0:ServiceProviderConfig";
const RESOURCE_TYPE_SCHEMA: &str = "urn:ietf:params:scim:schemas:core:2.0:ResourceType";
const SCHEMA_SCHEMA: &str = "urn:ietf:params:scim:schemas:core:2.0:Schema";

/// Most resources one list response returns (`filter.maxResults`).
pub const MAX_RESULTS: i64 = 1000;

/// Attributes never stored: server-assigned (`id`, `meta`), write-only (`password`), and
/// `active`, which is the user's deleted state.
const NOT_STORED: [&str; 4] = ["id", "meta", "password", "active"];

/// Renders a user as a SCIM User resource. `base` is the SCIM base URL (`…/.api/scim/v2`).
pub fn render(u: &ScimUser, base: &str) -> Value {
    let mut obj = match u.account.as_ref().map(|a| &a.data) {
        Some(Value::Object(data)) => data.clone(),
        _ => synthesize(u),
    };
    for key in NOT_STORED {
        remove_ci(&mut obj, key);
    }
    obj.insert("id".into(), Value::String(u.user.id.to_string()));
    let has_user_name = get_ci_map(&obj, "userName")
        .and_then(Value::as_str)
        .is_some_and(|s| !s.is_empty());
    if !has_user_name {
        remove_ci(&mut obj, "userName");
        obj.insert("userName".into(), Value::String(u.user.username.clone()));
    }
    obj.insert("active".into(), Value::Bool(u.user.deleted_at.is_none()));
    let has_emails = get_ci_map(&obj, "emails")
        .and_then(Value::as_array)
        .is_some_and(|a| !a.is_empty());
    if !has_emails && !u.emails.is_empty() {
        remove_ci(&mut obj, "emails");
        obj.insert("emails".into(), db_emails(u));
    }
    if get_ci_map(&obj, "displayName").is_none()
        && let Some(name) = &u.user.display_name
    {
        obj.insert("displayName".into(), Value::String(name.clone()));
    }
    let mut schemas = vec![Value::String(CORE_USER_SCHEMA.into())];
    if get_ci_map(&obj, ENTERPRISE_USER_SCHEMA).is_some() {
        schemas.push(Value::String(ENTERPRISE_USER_SCHEMA.into()));
    }
    remove_ci(&mut obj, "schemas");
    obj.insert("schemas".into(), Value::Array(schemas));

    let last_modified = u
        .account
        .as_ref()
        .map_or(u.user.updated_at, |a| a.updated_at.max(u.user.updated_at));
    let version = etag(&Value::Object(obj.clone()));
    obj.insert(
        "meta".into(),
        json!({
            "resourceType": "User",
            "created": u.user.created_at.to_rfc3339_opts(SecondsFormat::Millis, true),
            "lastModified": last_modified.to_rfc3339_opts(SecondsFormat::Millis, true),
            "location": format!("{base}/Users/{}", u.user.id),
            "version": version,
        }),
    );
    Value::Object(obj)
}

/// Attributes of a user the provider has not written (yet).
fn synthesize(u: &ScimUser) -> Map<String, Value> {
    let mut obj = Map::new();
    obj.insert("userName".into(), Value::String(u.user.username.clone()));
    if let Some(name) = &u.user.display_name {
        obj.insert("displayName".into(), Value::String(name.clone()));
        obj.insert("name".into(), json!({ "formatted": name }));
    }
    if !u.emails.is_empty() {
        obj.insert("emails".into(), db_emails(u));
    }
    obj
}

fn db_emails(u: &ScimUser) -> Value {
    Value::Array(
        u.emails
            .iter()
            .map(|e| json!({ "value": e.email, "primary": e.is_primary, "type": "work" }))
            .collect(),
    )
}

/// The weak entity tag of a rendered resource (without its `meta`).
pub fn etag(resource: &Value) -> String {
    let mut r = resource.clone();
    if let Some(obj) = r.as_object_mut() {
        obj.remove("meta");
    }
    let digest = Sha256::digest(serde_json::to_vec(&r).unwrap_or_default());
    format!("W/\"{}\"", &hex::encode(digest)[..20])
}

/// The `meta.version` of a rendered resource.
pub fn version_of(resource: &Value) -> &str {
    resource
        .pointer("/meta/version")
        .and_then(Value::as_str)
        .unwrap_or_default()
}

/// Coerces the string booleans some providers send (`"active": "False"`, `"primary": "true"`)
/// into JSON booleans.
pub fn coerce_booleans(resource: &mut Value) {
    let Some(obj) = resource.as_object_mut() else {
        return;
    };
    for (key, value) in obj.iter_mut() {
        if key.eq_ignore_ascii_case("active") {
            coerce_bool(value);
        } else if let Value::Array(items) = value {
            for item in items.iter_mut().filter_map(Value::as_object_mut) {
                for (k, v) in item.iter_mut() {
                    if k.eq_ignore_ascii_case("primary") {
                        coerce_bool(v);
                    }
                }
            }
        }
    }
}

fn coerce_bool(v: &mut Value) {
    if let Value::String(s) = v {
        if s.eq_ignore_ascii_case("true") {
            *v = Value::Bool(true);
        } else if s.eq_ignore_ascii_case("false") {
            *v = Value::Bool(false);
        }
    }
}

/// Reads the desired state out of a User resource. `current` is the user being replaced
/// (`None` on create): it supplies `active` when the resource has none and the emails of the
/// previous SCIM state.
pub fn to_write(resource: &Value, current: Option<&ScimUser>) -> Result<ScimWrite, ScimError> {
    let mut resource = resource.clone();
    coerce_booleans(&mut resource);
    let Value::Object(obj) = &resource else {
        return Err(ScimError::invalid_syntax(
            "the resource must be a JSON object",
        ));
    };
    if let Some(schemas) = get_ci_map(obj, "schemas") {
        let ok = schemas.as_array().is_some_and(|a| {
            a.iter().any(|s| {
                s.as_str()
                    .is_some_and(|s| s.eq_ignore_ascii_case(CORE_USER_SCHEMA))
            })
        });
        if !ok {
            return Err(ScimError::invalid_value(format!(
                "schemas must include {CORE_USER_SCHEMA}"
            )));
        }
    }
    let scim_user_name = match get_ci_map(obj, "userName") {
        Some(Value::String(s)) if !s.trim().is_empty() => s.trim().to_owned(),
        _ => return Err(ScimError::invalid_value("userName is required")),
    };
    let active = match get_ci_map(obj, "active") {
        None | Some(Value::Null) => current.is_none_or(|c| c.user.deleted_at.is_none()),
        Some(Value::Bool(b)) => *b,
        Some(_) => return Err(ScimError::invalid_value("active must be a boolean")),
    };
    let emails = emails_of(obj)?;
    for email in &emails {
        if sourcegull_authn::validate::validate_email(email).is_err() {
            return Err(ScimError::invalid_value(format!(
                "invalid email address {email:?}"
            )));
        }
    }
    let previous_emails = current
        .and_then(|c| c.account.as_ref())
        .and_then(|a| a.data.as_object())
        .map(emails_of)
        .transpose()?
        .unwrap_or_default();
    for (key, kind) in [("displayName", "string"), ("name", "object")] {
        let ok = match get_ci_map(obj, key) {
            None | Some(Value::Null) => true,
            Some(v) => (kind == "string" && v.is_string()) || (kind == "object" && v.is_object()),
        };
        if !ok {
            return Err(ScimError::invalid_value(format!("{key} must be a {kind}")));
        }
    }
    let external_id = match get_ci_map(obj, "externalId") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        Some(_) => return Err(ScimError::invalid_value("externalId must be a string")),
    };
    let mut data = obj.clone();
    for key in NOT_STORED {
        remove_ci(&mut data, key);
    }
    Ok(ScimWrite {
        username: normalize_username(&scim_user_name),
        account_id: external_id.unwrap_or_else(|| scim_user_name.to_lowercase()),
        display_name: display_name(obj),
        scim_user_name,
        emails,
        previous_emails,
        data: Value::Object(data),
        active,
    })
}

/// The addresses of `emails`, primary first, without duplicates.
fn emails_of(obj: &Map<String, Value>) -> Result<Vec<String>, ScimError> {
    let items = match get_ci_map(obj, "emails") {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(Value::Array(items)) => items,
        Some(_) => return Err(ScimError::invalid_value("emails must be an array")),
    };
    let mut primary = Vec::new();
    let mut others = Vec::new();
    for item in items {
        let Some(value) = get_ci(item, "value").and_then(Value::as_str) else {
            return Err(ScimError::invalid_value("every email needs a string value"));
        };
        let value = value.trim().to_owned();
        if get_ci(item, "primary").and_then(Value::as_bool) == Some(true) && primary.is_empty() {
            primary.push(value);
        } else {
            others.push(value);
        }
    }
    let mut out: Vec<String> = Vec::new();
    for e in primary.into_iter().chain(others) {
        if !e.is_empty() && !out.iter().any(|o| o.eq_ignore_ascii_case(&e)) {
            out.push(e);
        }
    }
    Ok(out)
}

/// `displayName`, else `name.formatted`, else given (middle) family names, else `nickName`.
fn display_name(obj: &Map<String, Value>) -> Option<String> {
    let s = |v: Option<&Value>| {
        v.and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    if let Some(d) = s(get_ci_map(obj, "displayName")) {
        return Some(d);
    }
    if let Some(name) = get_ci_map(obj, "name") {
        if let Some(f) = s(get_ci(name, "formatted")) {
            return Some(f);
        }
        let parts: Vec<String> = ["givenName", "middleName", "familyName"]
            .iter()
            .filter_map(|k| s(get_ci(name, k)))
            .collect();
        if !parts.is_empty() {
            return Some(parts.join(" "));
        }
    }
    s(get_ci_map(obj, "nickName"))
}

/// A valid Sourcegull username for a SCIM `userName`: letters, digits and `_` kept, `.` and `-`
/// kept between other characters, everything else (`@`, spaces, …) turned into `-`.
pub fn normalize_username(user_name: &str) -> String {
    let mut out = String::new();
    for c in user_name.chars() {
        let c = if c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-' {
            c
        } else {
            '-'
        };
        let sep = c == '-' || c == '.';
        if sep && (out.is_empty() || out.ends_with(['-', '.'])) {
            continue;
        }
        out.push(c);
    }
    let out: String = out.chars().take(200).collect();
    let out = out.trim_end_matches(['-', '.']).to_owned();
    if sourcegull_authn::validate::validate_username(&out).is_ok() {
        out
    } else {
        "user".into()
    }
}

/// Applies the `attributes` / `excludedAttributes` query parameters (RFC 7644 §3.4.2.5).
/// `id` and `schemas` are always returned.
pub fn project(resource: Value, attributes: &[AttrPath], excluded: &[AttrPath]) -> Value {
    let Value::Object(mut obj) = resource else {
        return resource;
    };
    if !attributes.is_empty() {
        let mut out = Map::new();
        for key in ["schemas", "id"] {
            if let Some(v) = obj.get(key) {
                out.insert(key.into(), v.clone());
            }
        }
        for path in attributes {
            copy_path(&obj, &mut out, path);
        }
        obj = out;
    }
    for path in excluded {
        if path.is("id") || path.is("schemas") {
            continue;
        }
        let container = match &path.extension {
            None => Some(&mut obj),
            Some(urn) => get_ci_map_mut(&mut obj, urn).and_then(Value::as_object_mut),
        };
        let Some(container) = container else { continue };
        match &path.sub {
            None => remove_ci(container, &path.attr),
            Some(sub) => match get_ci_map_mut(container, &path.attr) {
                Some(Value::Object(o)) => remove_ci(o, sub),
                Some(Value::Array(items)) => {
                    for o in items.iter_mut().filter_map(Value::as_object_mut) {
                        remove_ci(o, sub);
                    }
                }
                _ => {}
            },
        }
    }
    Value::Object(obj)
}

fn copy_path(from: &Map<String, Value>, to: &mut Map<String, Value>, path: &AttrPath) {
    let (src, dst) = match &path.extension {
        None => (Some(from), to),
        Some(urn) => {
            let Some((key, Value::Object(src))) = find_ci(from, urn) else {
                return;
            };
            let dst = to
                .entry(key.clone())
                .or_insert_with(|| Value::Object(Map::new()));
            let Some(dst) = dst.as_object_mut() else {
                return;
            };
            (Some(src), dst)
        }
    };
    let Some((key, value)) = src.and_then(|s| find_ci(s, &path.attr)) else {
        return;
    };
    match &path.sub {
        None => {
            dst.insert(key.clone(), value.clone());
        }
        Some(sub) => {
            let pick = |v: &Value| -> Option<Value> {
                let (k, x) = find_ci(v.as_object()?, sub)?;
                let mut m = Map::new();
                m.insert(k.clone(), x.clone());
                Some(Value::Object(m))
            };
            match value {
                Value::Object(_) => {
                    if let Some(Value::Object(picked)) = pick(value) {
                        let entry = dst
                            .entry(key.clone())
                            .or_insert_with(|| Value::Object(Map::new()));
                        if let Some(e) = entry.as_object_mut() {
                            e.extend(picked);
                        }
                    }
                }
                Value::Array(items) => {
                    let picked: Vec<Value> = items.iter().filter_map(pick).collect();
                    dst.insert(key.clone(), Value::Array(picked));
                }
                _ => {}
            }
        }
    }
}

fn find_ci<'a>(obj: &'a Map<String, Value>, key: &str) -> Option<(&'a String, &'a Value)> {
    obj.get_key_value(key)
        .or_else(|| obj.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)))
}

/// Case-insensitive lookup in a map.
pub(super) fn get_ci_map<'a>(obj: &'a Map<String, Value>, key: &str) -> Option<&'a Value> {
    find_ci(obj, key).map(|(_, v)| v)
}

/// Case-insensitive mutable lookup in a map.
pub(super) fn get_ci_map_mut<'a>(
    obj: &'a mut Map<String, Value>,
    key: &str,
) -> Option<&'a mut Value> {
    let actual = find_ci(obj, key).map(|(k, _)| k.clone())?;
    obj.get_mut(&actual)
}

/// Removes every key equal to `key` case-insensitively.
pub(super) fn remove_ci(obj: &mut Map<String, Value>, key: &str) {
    obj.retain(|k, _| !k.eq_ignore_ascii_case(key));
}

/// `ServiceProviderConfig` (RFC 7643 §5).
pub fn service_provider_config(base: &str) -> Value {
    json!({
        "schemas": [SERVICE_PROVIDER_CONFIG_SCHEMA],
        "documentationUri": "https://datatracker.ietf.org/doc/html/rfc7644",
        "patch": { "supported": true },
        "bulk": { "supported": false, "maxOperations": 0, "maxPayloadSize": 0 },
        "filter": { "supported": true, "maxResults": MAX_RESULTS },
        "changePassword": { "supported": false },
        "sort": { "supported": false },
        "etag": { "supported": true },
        "authenticationSchemes": [{
            "type": "oauthbearertoken",
            "name": "OAuth Bearer Token",
            "description": "The token set as scim.authToken in the site configuration, sent as \
                            Authorization: Bearer <token>.",
            "specUri": "https://www.rfc-editor.org/info/rfc6750",
            "primary": true
        }],
        "meta": {
            "resourceType": "ServiceProviderConfig",
            "location": format!("{base}/ServiceProviderConfig")
        }
    })
}

/// The User resource type (RFC 7643 §6).
pub fn user_resource_type(base: &str) -> Value {
    json!({
        "schemas": [RESOURCE_TYPE_SCHEMA],
        "id": "User",
        "name": "User",
        "endpoint": "/Users",
        "description": "User Account",
        "schema": CORE_USER_SCHEMA,
        "schemaExtensions": [{ "schema": ENTERPRISE_USER_SCHEMA, "required": false }],
        "meta": { "resourceType": "ResourceType", "location": format!("{base}/ResourceTypes/User") }
    })
}

/// The schema definitions served at `/Schemas` (RFC 7643 §7), limited to what Sourcegull reads.
/// Other attributes a provider sends are stored and returned as given.
pub fn schemas(base: &str) -> Vec<Value> {
    let s = |name: &str, desc: &str| attr(name, "string", false, desc, &[]);
    let user = json!({
        "schemas": [SCHEMA_SCHEMA],
        "id": CORE_USER_SCHEMA,
        "name": "User",
        "description": "User Account",
        "attributes": [
            {
                "name": "userName", "type": "string", "multiValued": false,
                "description": "Unique identifier for the user, used to derive the Sourcegull \
                                username.",
                "required": true, "caseExact": false, "mutability": "readWrite",
                "returned": "default", "uniqueness": "server"
            },
            attr("name", "complex", false, "The components of the user's name.", &[
                s("formatted", "The full name."),
                s("familyName", "The family name."),
                s("givenName", "The given name."),
                s("middleName", "The middle name."),
                s("honorificPrefix", "The honorific prefix."),
                s("honorificSuffix", "The honorific suffix."),
            ]),
            s("displayName", "The name shown in Sourcegull."),
            s("nickName", "The casual name."),
            s("title", "The user's title."),
            s("userType", "The relationship to the organization."),
            s("preferredLanguage", "The preferred language."),
            s("locale", "The default location."),
            s("timezone", "The time zone (IANA)."),
            attr("active", "boolean", false, "Whether the user can sign in. false \
                  deactivates the user (soft delete).", &[]),
            attr("emails", "complex", true, "Email addresses; all are stored verified, the \
                  primary one becomes the primary address.", &[
                s("value", "The address."),
                s("display", "A display name."),
                s("type", "work, home or other."),
                attr("primary", "boolean", false, "The primary address.", &[]),
            ]),
            attr("phoneNumbers", "complex", true, "Phone numbers (stored only).", &[
                s("value", "The number."),
                s("type", "work, home, mobile, fax, pager or other."),
                attr("primary", "boolean", false, "The primary number.", &[]),
            ]),
        ],
        "meta": {
            "resourceType": "Schema",
            "location": format!("{base}/Schemas/{CORE_USER_SCHEMA}")
        }
    });
    let enterprise = json!({
        "schemas": [SCHEMA_SCHEMA],
        "id": ENTERPRISE_USER_SCHEMA,
        "name": "EnterpriseUser",
        "description": "Enterprise User (stored only)",
        "attributes": [
            s("employeeNumber", "The employee number."),
            s("costCenter", "The cost center."),
            s("organization", "The organization."),
            s("division", "The division."),
            s("department", "The department."),
            attr("manager", "complex", false, "The user's manager.", &[
                s("value", "The manager's id."),
                s("displayName", "The manager's display name."),
            ]),
        ],
        "meta": {
            "resourceType": "Schema",
            "location": format!("{base}/Schemas/{ENTERPRISE_USER_SCHEMA}")
        }
    });
    vec![user, enterprise]
}

fn attr(name: &str, kind: &str, multi: bool, desc: &str, sub: &[Value]) -> Value {
    let mut v = json!({
        "name": name, "type": kind, "multiValued": multi, "description": desc,
        "required": false, "caseExact": false, "mutability": "readWrite",
        "returned": "default", "uniqueness": "none"
    });
    if !sub.is_empty() {
        v["subAttributes"] = Value::Array(sub.to_vec());
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usernames() {
        assert_eq!(normalize_username("alice@example.com"), "alice-example.com");
        assert_eq!(normalize_username("Bob Smith"), "Bob-Smith");
        assert_eq!(normalize_username("..x--y..@"), "x-y");
        assert_eq!(normalize_username("@@@"), "user");
        assert_eq!(normalize_username("dana_k"), "dana_k");
    }

    #[test]
    fn reads_desired_state() {
        let r = json!({
            "schemas": [CORE_USER_SCHEMA],
            "userName": "alice@example.com",
            "externalId": "00u1",
            "name": {"givenName": "Alice", "familyName": "Liddell"},
            "emails": [
                {"value": "a2@example.com", "type": "home"},
                {"value": "alice@example.com", "type": "work", "primary": "True"},
                {"value": "ALICE@example.com"}
            ],
            "active": "true",
            "password": "secret",
            "id": "999"
        });
        let w = to_write(&r, None).unwrap();
        assert_eq!(w.username, "alice-example.com");
        assert_eq!(w.account_id, "00u1");
        assert_eq!(w.display_name.as_deref(), Some("Alice Liddell"));
        assert_eq!(w.emails, ["alice@example.com", "a2@example.com"]);
        assert!(w.active);
        for gone in ["password", "id", "active"] {
            assert!(w.data.get(gone).is_none(), "{gone} is not stored");
        }

        let no_name = json!({"emails": []});
        assert_eq!(
            to_write(&no_name, None).unwrap_err().scim_type,
            Some("invalidValue")
        );
        let bad_email = json!({"userName": "x", "emails": [{"value": "nope"}]});
        assert!(to_write(&bad_email, None).is_err());
        let bad_schema = json!({"schemas": ["urn:x"], "userName": "x"});
        assert!(to_write(&bad_schema, None).is_err());
        let no_external = to_write(&json!({"userName": "Carol"}), None).unwrap();
        assert_eq!(no_external.account_id, "carol");
    }

    #[test]
    fn projection() {
        let r = json!({
            "schemas": [CORE_USER_SCHEMA], "id": "1", "userName": "a",
            "name": {"givenName": "A", "familyName": "B"},
            "emails": [{"value": "a@b.c", "type": "work"}],
            "meta": {"resourceType": "User"}
        });
        let p = |a: &[&str], e: &[&str]| {
            let parse = |v: &[&str]| {
                v.iter()
                    .map(|s| AttrPath::parse(s).unwrap())
                    .collect::<Vec<_>>()
            };
            project(r.clone(), &parse(a), &parse(e))
        };
        assert_eq!(
            p(&["userName", "name.givenName", "emails.value"], &[]),
            json!({"schemas": [CORE_USER_SCHEMA], "id": "1", "userName": "a",
                   "name": {"givenName": "A"}, "emails": [{"value": "a@b.c"}]})
        );
        let ex = p(&[], &["emails", "name.familyName", "id"]);
        assert!(ex.get("emails").is_none());
        assert_eq!(ex["name"], json!({"givenName": "A"}));
        assert_eq!(ex["id"], "1");
    }
}
