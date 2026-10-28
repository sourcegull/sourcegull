//! The JSON Schemas of bulk change specs and changeset specs and the translation of `jsonschema` errors
//! into [`SpecIssue`]s with source positions.

use std::sync::LazyLock;

use jsonschema::error::{TypeKind, ValidationErrorKind};
use jsonschema::{ValidationError, Validator};
use serde_json::Value;

use crate::error::{SpecIssue, with_field};
use crate::yaml::{Positions, escape_pointer_segment};

/// The bulk change spec JSON Schema (draft 7), as checked in.
pub const BATCH_SPEC_SCHEMA: &str = include_str!("../schema/batch_spec.schema.json");
/// The changeset spec JSON Schema (draft 7), as checked in.
pub const CHANGESET_SPEC_SCHEMA: &str = include_str!("../schema/changeset_spec.schema.json");

/// The message for a `name` that does not match the pattern.
pub const NAME_PATTERN_MESSAGE: &str = "The bulk change name can only contain word characters, dots and dashes. No whitespace or newlines allowed.";

fn compile(text: &str) -> Result<Validator, String> {
    let schema: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
    jsonschema::draft7::options()
        .should_validate_formats(true)
        .build(&schema)
        .map_err(|e| e.to_string())
}

pub(crate) static BATCH_SPEC: LazyLock<Result<Validator, String>> =
    LazyLock::new(|| compile(BATCH_SPEC_SCHEMA));
pub(crate) static CHANGESET_SPEC: LazyLock<Result<Validator, String>> =
    LazyLock::new(|| compile(CHANGESET_SPEC_SCHEMA));

/// Validates `value` and describes every violation, located through `positions`.
pub(crate) fn validate(
    validator: &LazyLock<Result<Validator, String>>,
    value: &Value,
    positions: &Positions,
) -> Vec<SpecIssue> {
    let validator = match validator.as_ref() {
        Ok(v) => v,
        Err(e) => {
            return vec![SpecIssue::new(
                "",
                format!("internal error: the schema does not compile: {e}"),
            )];
        }
    };
    let mut issues = Vec::new();
    for error in validator.iter_errors(value) {
        describe(&error, positions, &mut issues);
    }
    issues
}

fn json_type(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(n) if n.is_f64() => "number",
        Value::Number(_) => "integer",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn plural(n: u64, one: &str, many: &str) -> String {
    if n == 1 {
        format!("{n} {one}")
    } else {
        format!("{n} {many}")
    }
}

fn describe(error: &ValidationError<'_>, positions: &Positions, out: &mut Vec<SpecIssue>) {
    let pointer = error.instance_path.to_string();
    let issue = |path: &str, message: String, key: bool| {
        let location = if key {
            positions.key(path)
        } else {
            positions.value(path)
        };
        SpecIssue::new(path, message).at(location)
    };
    let plain = |message: &str| issue(&pointer, with_field(&pointer, message), false);
    match &error.kind {
        ValidationErrorKind::AdditionalProperties { unexpected } => {
            for name in unexpected {
                let path = format!("{pointer}/{}", escape_pointer_segment(name));
                out.push(issue(
                    &path,
                    with_field(
                        &pointer,
                        &format!("additional property {name:?} is not allowed"),
                    ),
                    true,
                ));
            }
        }
        ValidationErrorKind::Required { property } => {
            let name = property
                .as_str()
                .map_or_else(|| property.to_string(), str::to_string);
            out.push(plain(&format!("property {name:?} is required")));
        }
        ValidationErrorKind::Type { .. } if pointer.is_empty() && error.instance.is_null() => {
            out.push(plain("the document is empty"));
        }
        ValidationErrorKind::Type { kind } => {
            let expected: Vec<String> = match kind {
                TypeKind::Single(t) => vec![t.to_string()],
                TypeKind::Multiple(set) => set.iter().map(|t| t.to_string()).collect(),
            };
            let expected = match expected.as_slice() {
                [one] => one.clone(),
                [init @ .., last] => format!("{} or {last}", init.join(", ")),
                [] => "another type".to_string(),
            };
            out.push(plain(&format!(
                "must be of type {expected}, not {}",
                json_type(&error.instance)
            )));
        }
        ValidationErrorKind::Enum { options } => {
            let options = match options {
                Value::Array(items) => items
                    .iter()
                    .map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_string))
                    .collect::<Vec<_>>()
                    .join(", "),
                other => other.to_string(),
            };
            out.push(plain(&format!("must be one of the following: {options}")));
        }
        ValidationErrorKind::Pattern { pattern } => {
            if pointer == "/name" {
                out.push(issue(&pointer, NAME_PATTERN_MESSAGE.to_string(), false));
            } else if pattern.starts_with("^refs\\/heads\\/") {
                out.push(plain("must be a full branch ref (refs/heads/<branch>)"));
            } else if pattern == "^draft$" {
                out.push(plain("must be true, false or \"draft\""));
            } else {
                out.push(plain(&format!("must match the pattern {pattern}")));
            }
        }
        ValidationErrorKind::Format { format } => {
            let what = if format == "email" {
                "an email address".to_string()
            } else {
                format!("a valid {format}")
            };
            out.push(plain(&format!("must be {what}")));
        }
        ValidationErrorKind::MinLength { limit } => {
            if *limit == 1 {
                out.push(plain("must not be empty"));
            } else {
                out.push(plain(&format!(
                    "must be at least {} long",
                    plural(*limit, "character", "characters")
                )));
            }
        }
        ValidationErrorKind::MinProperties { limit } => {
            out.push(plain(&format!(
                "must have at least {}",
                plural(*limit, "property", "properties")
            )));
        }
        ValidationErrorKind::MaxProperties { limit } => {
            out.push(plain(&format!(
                "must have at most {}",
                plural(*limit, "property", "properties")
            )));
        }
        ValidationErrorKind::MinItems { limit } => {
            out.push(plain(&format!(
                "must have at least {}",
                plural(*limit, "item", "items")
            )));
        }
        ValidationErrorKind::MaxItems { limit } => {
            out.push(plain(&format!(
                "must have at most {}",
                plural(*limit, "item", "items")
            )));
        }
        ValidationErrorKind::UniqueItems => {
            // Point at the first repeated item.
            let mut path = pointer.clone();
            if let Value::Array(items) = error.instance.as_ref() {
                if let Some(i) = (1..items.len()).find(|&i| items[..i].contains(&items[i])) {
                    path = format!("{pointer}/{i}");
                }
            }
            out.push(issue(
                &path,
                with_field(&pointer, "must not contain duplicate items"),
                false,
            ));
        }
        ValidationErrorKind::Not { .. }
            if error
                .instance
                .as_object()
                .is_some_and(|o| o.contains_key("branch") && o.contains_key("branches")) =>
        {
            // The only `not` in the bulk change spec schema: `on` with branch and branches.
            out.push(plain("both branch and branches specified"));
        }
        _ => out.push(plain(&error.to_string())),
    }
}
