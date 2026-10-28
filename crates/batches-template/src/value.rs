//! The dynamic values templates operate on, modelled on the Go values of template contexts
//! (`string`, `int`, `float64`, `bool`, `[]string`, `[]any`, `map[string]any`, `nil`).

use std::collections::BTreeMap;

/// A template value.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum Value {
    /// Go's `nil` (an unset `any`, a JSON `null`). Printed as `<no value>` when it is an
    /// action's result and as `<nil>` inside lists, maps and `print`.
    #[default]
    Nil,
    Bool(bool),
    Int(i64),
    Float(f64),
    String(String),
    /// A slice (`[]string` or `[]any`); printed as `[a b c]`.
    List(Vec<Value>),
    /// A `map[string]any`; fields are looked up by key and it prints as `map[k:v]` with
    /// sorted keys.
    Map(BTreeMap<String, Value>),
    /// A list of paths that prints space-separated (`repository.search_result_paths`), while still
    /// being a `[]string` for `join`, `index`, `len` and `range`.
    PathList(Vec<String>),
}

impl Value {
    /// Go's truth (`if`, `with`, `and`, `or`, `not`): false for nil, `false`, zero numbers
    /// and empty strings, lists and maps.
    pub fn is_true(&self) -> bool {
        match self {
            Value::Nil => false,
            Value::Bool(b) => *b,
            Value::Int(i) => *i != 0,
            Value::Float(f) => *f != 0.0,
            Value::String(s) => !s.is_empty(),
            Value::List(l) => !l.is_empty(),
            Value::Map(m) => !m.is_empty(),
            Value::PathList(l) => !l.is_empty(),
        }
    }

    /// The Go type name used in error messages.
    pub fn go_type(&self) -> &'static str {
        match self {
            Value::Nil => "<nil>",
            Value::Bool(_) => "bool",
            Value::Int(_) => "int",
            Value::Float(_) => "float64",
            Value::String(_) => "string",
            Value::List(_) => "[]interface {}",
            Value::Map(_) => "map[string]interface {}",
            Value::PathList(_) => "[]string",
        }
    }

    /// The elements of a list whose elements are all strings (what a Go `[]string`
    /// parameter accepts); `nil` is an empty list.
    pub(crate) fn as_string_list(&self) -> Option<Vec<String>> {
        match self {
            Value::Nil => Some(Vec::new()),
            Value::PathList(l) => Some(l.clone()),
            Value::List(l) => l
                .iter()
                .map(|v| match v {
                    Value::String(s) => Some(s.clone()),
                    _ => None,
                })
                .collect(),
            _ => None,
        }
    }

    /// A list of strings (`[]string`).
    pub fn strings<I, S>(items: I) -> Value
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Value::List(items.into_iter().map(|s| Value::String(s.into())).collect())
    }

    /// A map from `(key, value)` pairs.
    pub fn map<I, K>(entries: I) -> Value
    where
        I: IntoIterator<Item = (K, Value)>,
        K: Into<String>,
    {
        Value::Map(entries.into_iter().map(|(k, v)| (k.into(), v)).collect())
    }

    /// Converts JSON the way Go's `encoding/json` decodes into `any`: every number is a
    /// `float64` (so `eq outputs.count 3` is a type error in both), objects are maps.
    pub fn from_json(json: &serde_json::Value) -> Value {
        match json {
            serde_json::Value::Null => Value::Nil,
            serde_json::Value::Bool(b) => Value::Bool(*b),
            serde_json::Value::Number(n) => Value::Float(n.as_f64().unwrap_or(0.0)),
            serde_json::Value::String(s) => Value::String(s.clone()),
            serde_json::Value::Array(a) => Value::List(a.iter().map(Value::from_json).collect()),
            serde_json::Value::Object(o) => Value::Map(
                o.iter()
                    .map(|(k, v)| (k.clone(), Value::from_json(v)))
                    .collect(),
            ),
        }
    }

    /// Converts JSON keeping integral numbers as `int`, as YAML scalars of `format: yaml`
    /// outputs are decoded.
    pub fn from_json_yaml_numbers(json: &serde_json::Value) -> Value {
        match json {
            serde_json::Value::Number(n) => match n.as_i64() {
                Some(i) => Value::Int(i),
                None => Value::Float(n.as_f64().unwrap_or(0.0)),
            },
            serde_json::Value::Array(a) => {
                Value::List(a.iter().map(Value::from_json_yaml_numbers).collect())
            }
            serde_json::Value::Object(o) => Value::Map(
                o.iter()
                    .map(|(k, v)| (k.clone(), Value::from_json_yaml_numbers(v)))
                    .collect(),
            ),
            other => Value::from_json(other),
        }
    }

    /// The value as JSON (path lists become arrays; non-finite floats become `null`).
    pub fn to_json(&self) -> serde_json::Value {
        match self {
            Value::Nil => serde_json::Value::Null,
            Value::Bool(b) => serde_json::Value::Bool(*b),
            Value::Int(i) => serde_json::Value::from(*i),
            Value::Float(f) => serde_json::Number::from_f64(*f)
                .map(serde_json::Value::Number)
                .unwrap_or(serde_json::Value::Null),
            Value::String(s) => serde_json::Value::String(s.clone()),
            Value::List(l) => serde_json::Value::Array(l.iter().map(Value::to_json).collect()),
            Value::PathList(l) => serde_json::Value::Array(
                l.iter()
                    .map(|s| serde_json::Value::String(s.clone()))
                    .collect(),
            ),
            Value::Map(m) => {
                serde_json::Value::Object(m.iter().map(|(k, v)| (k.clone(), v.to_json())).collect())
            }
        }
    }
}

impl From<&str> for Value {
    fn from(s: &str) -> Self {
        Value::String(s.to_owned())
    }
}

impl From<String> for Value {
    fn from(s: String) -> Self {
        Value::String(s)
    }
}

impl From<bool> for Value {
    fn from(b: bool) -> Self {
        Value::Bool(b)
    }
}

impl From<i64> for Value {
    fn from(i: i64) -> Self {
        Value::Int(i)
    }
}

impl From<f64> for Value {
    fn from(f: f64) -> Self {
        Value::Float(f)
    }
}

impl From<Vec<Value>> for Value {
    fn from(l: Vec<Value>) -> Self {
        Value::List(l)
    }
}

impl From<BTreeMap<String, Value>> for Value {
    fn from(m: BTreeMap<String, Value>) -> Self {
        Value::Map(m)
    }
}

impl std::fmt::Display for Value {
    /// Go's `%v`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut s = String::new();
        crate::gofmt::write_v(self, &mut s);
        f.write_str(&s)
    }
}
