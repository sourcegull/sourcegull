//! SCIM PATCH (RFC 7644 §3.5.2): `add`, `replace` and `remove` operations applied to a
//! rendered resource. The result is then written like a PUT.
//!
//! Lenient where identity providers differ from the RFC: operation names are
//! case-insensitive (`Replace`), string booleans are accepted (`"value": "False"`), keys of a
//! path-less value may themselves be paths (`{"name.givenName": "A"}`), and `add`/`replace`
//! through a value filter that matches nothing adds an element built from the filter's `eq`
//! terms (`emails[type eq "work"].value`).

use serde_json::{Map, Value};

use super::ScimError;
use super::filter::{self, AttrPath, CompareOp, Filter};
use super::resource::{
    CORE_USER_SCHEMA, ENTERPRISE_USER_SCHEMA, PATCH_OP_SCHEMA, get_ci_map, get_ci_map_mut,
    remove_ci,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Add,
    Replace,
    Remove,
}

/// A PATCH path: `attrPath` or `attrPath[filter][.sub]`.
#[derive(Debug, Clone)]
struct Path {
    attr: AttrPath,
    filter: Option<Filter>,
    /// The sub-attribute after a value filter.
    sub: Option<String>,
}

/// Applies a PATCH request body to `resource`.
pub fn apply(resource: &mut Value, body: &Value) -> Result<(), ScimError> {
    let Value::Object(body) = body else {
        return Err(ScimError::invalid_syntax(
            "the request must be a JSON object",
        ));
    };
    if let Some(schemas) = get_ci_map(body, "schemas") {
        let ok = schemas.as_array().is_some_and(|a| {
            a.iter().any(|s| {
                s.as_str()
                    .is_some_and(|s| s.eq_ignore_ascii_case(PATCH_OP_SCHEMA))
            })
        });
        if !ok {
            return Err(ScimError::invalid_syntax(format!(
                "schemas must include {PATCH_OP_SCHEMA}"
            )));
        }
    }
    let Some(Value::Array(ops)) = get_ci_map(body, "Operations") else {
        return Err(ScimError::invalid_syntax("Operations must be an array"));
    };
    if ops.is_empty() {
        return Err(ScimError::invalid_syntax("Operations must not be empty"));
    }
    let Value::Object(root) = resource else {
        return Err(ScimError::internal("resource is not an object"));
    };
    for op in ops {
        let op_name = filter::get_ci(op, "op")
            .and_then(Value::as_str)
            .ok_or_else(|| ScimError::invalid_syntax("every operation needs an op"))?;
        let kind = match op_name.to_ascii_lowercase().as_str() {
            "add" => Op::Add,
            "replace" => Op::Replace,
            "remove" => Op::Remove,
            other => {
                return Err(ScimError::invalid_syntax(format!(
                    "unsupported op {other:?}"
                )));
            }
        };
        let path = match filter::get_ci(op, "path") {
            None | Some(Value::Null) => None,
            Some(Value::String(p)) if p.trim().is_empty() => None,
            Some(Value::String(p)) => Some(p.trim().to_owned()),
            Some(_) => return Err(ScimError::invalid_path("path must be a string")),
        };
        let value = filter::get_ci(op, "value").cloned();
        match path {
            None => {
                if kind == Op::Remove {
                    return Err(ScimError::no_target("remove requires a path"));
                }
                let Some(Value::Object(attrs)) = value else {
                    return Err(ScimError::invalid_value(
                        "an operation without a path needs an object value",
                    ));
                };
                apply_object(root, kind, attrs)?;
            }
            Some(p) => {
                if kind != Op::Remove && value.is_none() {
                    return Err(ScimError::invalid_value(format!(
                        "{op_name} requires a value"
                    )));
                }
                apply_path(root, kind, &p, value.unwrap_or(Value::Null))?;
            }
        }
    }
    Ok(())
}

/// A path-less `add`/`replace`: each key is an attribute (or a path, or an extension schema
/// holding attributes).
fn apply_object(
    root: &mut Map<String, Value>,
    kind: Op,
    attrs: Map<String, Value>,
) -> Result<(), ScimError> {
    for (key, value) in attrs {
        if key.eq_ignore_ascii_case("schemas") {
            continue;
        }
        apply_path(root, kind, &key, value)?;
    }
    Ok(())
}

fn is_schema(s: &str) -> bool {
    [CORE_USER_SCHEMA, ENTERPRISE_USER_SCHEMA]
        .iter()
        .any(|schema| schema.eq_ignore_ascii_case(s))
}

fn parse_path(raw: &str) -> Result<Path, ScimError> {
    let invalid = |e: String| ScimError::invalid_path(format!("invalid path {raw:?}: {e}"));
    let Some(open) = raw.find('[') else {
        return Ok(Path {
            attr: AttrPath::parse(raw).map_err(invalid)?,
            filter: None,
            sub: None,
        });
    };
    let close = raw
        .rfind(']')
        .ok_or_else(|| invalid("missing ']'".into()))?;
    if close < open {
        return Err(invalid("misplaced ']'".into()));
    }
    let attr = AttrPath::parse(&raw[..open]).map_err(invalid)?;
    if attr.sub.is_some() {
        return Err(invalid("a value filter must follow an attribute".into()));
    }
    let filter = filter::parse(&raw[open + 1..close]).map_err(invalid)?;
    let rest = &raw[close + 1..];
    let sub = match rest.strip_prefix('.') {
        Some(s) if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') => {
            Some(s.to_owned())
        }
        None if rest.is_empty() => None,
        _ => return Err(invalid(format!("unexpected {rest:?}"))),
    };
    Ok(Path {
        attr,
        filter: Some(filter),
        sub,
    })
}

fn apply_path(
    root: &mut Map<String, Value>,
    kind: Op,
    raw: &str,
    value: Value,
) -> Result<(), ScimError> {
    // A whole extension schema: its attributes are applied one by one.
    if is_schema(raw) {
        if raw.eq_ignore_ascii_case(CORE_USER_SCHEMA) {
            return match value {
                Value::Object(attrs) if kind != Op::Remove => apply_object(root, kind, attrs),
                _ => Err(ScimError::invalid_path("cannot target the core schema")),
            };
        }
        if kind == Op::Remove {
            remove_ci(root, raw);
            return Ok(());
        }
        let Value::Object(attrs) = value else {
            return Err(ScimError::invalid_value(
                "an extension value must be an object",
            ));
        };
        for (key, v) in attrs {
            apply_path(root, kind, &format!("{ENTERPRISE_USER_SCHEMA}:{key}"), v)?;
        }
        return Ok(());
    }
    let path = parse_path(raw)?;
    if path.attr.is("id") {
        return Err(ScimError::mutability("id is read-only"));
    }
    if path.attr.is("meta") || path.attr.is("schemas") {
        // Read-only, server-managed attributes: ignored.
        return Ok(());
    }
    let container = match &path.attr.extension {
        None => root,
        Some(urn) => {
            if get_ci_map(root, urn).is_none() {
                if kind == Op::Remove {
                    return Ok(());
                }
                root.insert(canonical_schema(urn), Value::Object(Map::new()));
            }
            match get_ci_map_mut(root, urn) {
                Some(Value::Object(o)) => o,
                _ => return Err(ScimError::invalid_path(format!("{urn} is not an object"))),
            }
        }
    };
    match &path.filter {
        None => apply_plain(container, kind, &path.attr, value),
        Some(f) => apply_filtered(container, kind, &path, f, value),
    }
}

fn canonical_schema(urn: &str) -> String {
    if urn.eq_ignore_ascii_case(ENTERPRISE_USER_SCHEMA) {
        ENTERPRISE_USER_SCHEMA.to_owned()
    } else {
        urn.to_owned()
    }
}

/// The key under which `attr` is (or will be) stored: the existing spelling if any.
fn key_of(container: &Map<String, Value>, attr: &str) -> String {
    container
        .keys()
        .find(|k| k.eq_ignore_ascii_case(attr))
        .cloned()
        .unwrap_or_else(|| attr.to_owned())
}

fn apply_plain(
    container: &mut Map<String, Value>,
    kind: Op,
    attr: &AttrPath,
    value: Value,
) -> Result<(), ScimError> {
    let key = key_of(container, &attr.attr);
    let Some(sub) = &attr.sub else {
        match kind {
            Op::Remove => {
                container.remove(&key);
            }
            Op::Add => {
                let merged = match (container.remove(&key), value) {
                    (Some(Value::Array(mut existing)), Value::Array(new)) => {
                        for v in new {
                            if !existing.contains(&v) {
                                existing.push(v);
                            }
                        }
                        Value::Array(existing)
                    }
                    (Some(Value::Array(mut existing)), v) => {
                        if !existing.contains(&v) {
                            existing.push(v);
                        }
                        Value::Array(existing)
                    }
                    (Some(Value::Object(mut existing)), Value::Object(new)) => {
                        merge(&mut existing, new);
                        Value::Object(existing)
                    }
                    (_, v) => v,
                };
                container.insert(key, merged);
            }
            Op::Replace => {
                let replaced = match (container.remove(&key), value) {
                    (Some(Value::Object(mut existing)), Value::Object(new)) => {
                        merge(&mut existing, new);
                        Value::Object(existing)
                    }
                    (_, v) => v,
                };
                container.insert(key, replaced);
            }
        }
        return Ok(());
    };
    match container.get_mut(&key) {
        Some(Value::Array(items)) => {
            for item in items.iter_mut().filter_map(Value::as_object_mut) {
                set_sub(item, kind, sub, value.clone());
            }
        }
        Some(Value::Object(o)) => set_sub(o, kind, sub, value),
        _ if kind == Op::Remove => {}
        _ => {
            let mut o = Map::new();
            set_sub(&mut o, kind, sub, value);
            container.insert(key, Value::Object(o));
        }
    }
    Ok(())
}

fn apply_filtered(
    container: &mut Map<String, Value>,
    kind: Op,
    path: &Path,
    f: &Filter,
    value: Value,
) -> Result<(), ScimError> {
    let key = key_of(container, &path.attr.attr);
    let items = match container.get_mut(&key) {
        Some(Value::Array(items)) => items,
        None | Some(Value::Null) => {
            if kind == Op::Remove {
                return Ok(());
            }
            container.insert(key.clone(), Value::Array(Vec::new()));
            match container.get_mut(&key) {
                Some(Value::Array(items)) => items,
                _ => unreachable!("just inserted"),
            }
        }
        Some(_) => {
            return Err(ScimError::invalid_path(format!(
                "{} is not multi-valued",
                path.attr.attr
            )));
        }
    };
    let matching: Vec<usize> = items
        .iter()
        .enumerate()
        .filter(|(_, item)| filter::matches(f, item))
        .map(|(i, _)| i)
        .collect();
    match kind {
        Op::Remove => {
            match &path.sub {
                Some(sub) => {
                    for &i in &matching {
                        if let Some(o) = items[i].as_object_mut() {
                            remove_ci(o, sub);
                        }
                    }
                }
                None => {
                    let mut i = 0;
                    items.retain(|_| {
                        let keep = !matching.contains(&i);
                        i += 1;
                        keep
                    });
                }
            }
            if items.is_empty() {
                container.remove(&key);
            }
        }
        Op::Add | Op::Replace => {
            if matching.is_empty() {
                let mut element = Map::new();
                if !eq_terms(f, &mut element) {
                    return Err(ScimError::no_target(format!(
                        "no value of {} matches the filter",
                        path.attr.attr
                    )));
                }
                write_element(&mut element, kind, path.sub.as_deref(), value)?;
                items.push(Value::Object(element));
            } else {
                for &i in &matching {
                    let Some(element) = items[i].as_object_mut() else {
                        continue;
                    };
                    if kind == Op::Replace && path.sub.is_none() {
                        element.clear();
                    }
                    write_element(element, kind, path.sub.as_deref(), value.clone())?;
                }
            }
        }
    }
    Ok(())
}

fn write_element(
    element: &mut Map<String, Value>,
    kind: Op,
    sub: Option<&str>,
    value: Value,
) -> Result<(), ScimError> {
    match sub {
        Some(sub) => set_sub(element, kind, sub, value),
        None => {
            let Value::Object(v) = value else {
                return Err(ScimError::invalid_value(
                    "a value filter without a sub-attribute needs an object value",
                ));
            };
            merge(element, v);
        }
    }
    Ok(())
}

fn set_sub(o: &mut Map<String, Value>, kind: Op, sub: &str, value: Value) {
    let key = key_of(o, sub);
    if kind == Op::Remove {
        o.remove(&key);
    } else {
        o.insert(key, value);
    }
}

/// Merges `new` into `existing`, matching keys case-insensitively.
fn merge(existing: &mut Map<String, Value>, new: Map<String, Value>) {
    for (k, v) in new {
        let key = key_of(existing, &k);
        existing.insert(key, v);
    }
}

/// Fills `element` with the `attr eq value` terms of an `and` chain; `false` when the filter
/// has other terms.
fn eq_terms(f: &Filter, element: &mut Map<String, Value>) -> bool {
    match f {
        Filter::And(a, b) => eq_terms(a, element) && eq_terms(b, element),
        Filter::Compare(p, CompareOp::Eq, v)
            if p.extension.is_none() && p.sub.is_none() && !v.is_null() =>
        {
            element.insert(p.attr.clone(), v.clone());
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn user() -> Value {
        json!({
            "schemas": [CORE_USER_SCHEMA],
            "id": "7",
            "userName": "alice",
            "active": true,
            "name": {"givenName": "Alice", "familyName": "L"},
            "emails": [
                {"value": "alice@example.com", "type": "work", "primary": true},
                {"value": "a@home.example", "type": "home"}
            ]
        })
    }

    fn patch(ops: Value) -> Result<Value, ScimError> {
        let mut r = user();
        apply(
            &mut r,
            &json!({"schemas": [PATCH_OP_SCHEMA], "Operations": ops}),
        )?;
        Ok(r)
    }

    #[test]
    fn okta_and_entra_deactivation() {
        let r = patch(json!([{"op": "replace", "value": {"active": false}}])).unwrap();
        assert_eq!(r["active"], false);
        let r = patch(json!([{"op": "Replace", "path": "active", "value": "False"}])).unwrap();
        assert_eq!(r["active"], "False", "coerced when the resource is read");
    }

    #[test]
    fn entra_attribute_updates() {
        let r = patch(json!([
            {"op": "Replace", "path": "emails[type eq \"work\"].value", "value": "new@example.com"},
            {"op": "Add", "path": "name.givenName", "value": "Alicia"},
            {"op": "Add", "path": "phoneNumbers[type eq \"mobile\"].value", "value": "+1 555"},
            {"op": "Replace", "path": "urn:ietf:params:scim:schemas:extension:enterprise:2.0:User:department", "value": "Eng"},
            {"op": "Replace", "value": {"displayName": "Alicia L", "name.familyName": "Liddell"}}
        ]))
        .unwrap();
        assert_eq!(r["emails"][0]["value"], "new@example.com");
        assert_eq!(r["emails"][0]["primary"], true);
        assert_eq!(
            r["name"],
            json!({"givenName": "Alicia", "familyName": "Liddell"})
        );
        assert_eq!(
            r["phoneNumbers"],
            json!([{"type": "mobile", "value": "+1 555"}])
        );
        assert_eq!(r[ENTERPRISE_USER_SCHEMA]["department"], "Eng");
        assert_eq!(r["displayName"], "Alicia L");
    }

    #[test]
    fn add_replace_remove() {
        let r = patch(json!([
            {"op": "add", "path": "emails", "value": [{"value": "x@y.z", "type": "other"}]},
            {"op": "remove", "path": "emails[type eq \"home\"]"},
            {"op": "remove", "path": "name.familyName"},
            {"op": "add", "value": {"nickName": "Al", "name": {"middleName": "M"}}}
        ]))
        .unwrap();
        let emails: Vec<&str> = r["emails"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["value"].as_str().unwrap())
            .collect();
        assert_eq!(emails, ["alice@example.com", "x@y.z"]);
        assert_eq!(r["name"], json!({"givenName": "Alice", "middleName": "M"}));
        assert_eq!(r["nickName"], "Al");
        let r =
            patch(json!([{"op": "replace", "path": "emails", "value": [{"value": "only@x.y"}]}]))
                .unwrap();
        assert_eq!(r["emails"], json!([{"value": "only@x.y"}]));
        let r = patch(json!([{"op": "remove", "path": "emails"}])).unwrap();
        assert!(r.get("emails").is_none());
    }

    #[test]
    fn errors() {
        let err = |ops: Value| patch(ops).unwrap_err().scim_type;
        assert_eq!(err(json!([{"op": "remove"}])), Some("noTarget"));
        assert_eq!(
            err(json!([{"op": "replace", "path": "id", "value": "9"}])),
            Some("mutability")
        );
        assert_eq!(
            err(json!([{"op": "replace", "path": "emails[", "value": "x"}])),
            Some("invalidPath")
        );
        assert_eq!(
            err(json!([{"op": "move", "path": "x", "value": 1}])),
            Some("invalidSyntax")
        );
        assert_eq!(
            err(json!([{"op": "replace", "path": "emails[value co \"z\"].type", "value": "w"}])),
            Some("noTarget")
        );
        let mut r = user();
        assert!(apply(&mut r, &json!({"Operations": []})).is_err());
        assert!(apply(&mut r, &json!({"schemas": ["urn:x"], "Operations": [{"op": "add", "path": "a", "value": 1}]})).is_err());
    }
}
