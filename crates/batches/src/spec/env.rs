//! Step environments (`steps[].env`).
//!
//! Two forms are accepted: a map of names to values, or a list whose entries are either a bare
//! `NAME` (the value is passed through from outside: the executor secrets of the namespace on
//! server-side runs, the local environment with `gull bulk`) or a one-entry `{NAME: value}` map.

use std::collections::{BTreeMap, HashMap};

use serde::de::{self, Deserializer};
use serde::ser::{SerializeMap, SerializeSeq, Serializer};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One variable; `value` is `None` for pass-through variables.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvVar {
    pub name: String,
    pub value: Option<String>,
}

/// The environment of a step, in declaration order (map form: sorted by name).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Environment {
    vars: Vec<EnvVar>,
}

impl Environment {
    pub fn new(vars: Vec<EnvVar>) -> Self {
        Environment { vars }
    }

    pub fn vars(&self) -> &[EnvVar] {
        &self.vars
    }

    pub fn is_empty(&self) -> bool {
        self.vars.is_empty()
    }

    /// Whether no variable needs a value from outside.
    pub fn is_static(&self) -> bool {
        self.vars.iter().all(|v| v.value.is_some())
    }

    /// Names of the pass-through variables, in order.
    pub fn outer_vars(&self) -> Vec<&str> {
        self.vars
            .iter()
            .filter(|v| v.value.is_none())
            .map(|v| v.name.as_str())
            .collect()
    }

    /// The final environment: pass-through variables take their value from `outer` (empty when
    /// missing there). A later entry for the same name wins.
    pub fn resolve(&self, outer: &HashMap<String, String>) -> BTreeMap<String, String> {
        self.vars
            .iter()
            .map(|v| {
                let value = match &v.value {
                    Some(value) => value.clone(),
                    None => outer.get(&v.name).cloned().unwrap_or_default(),
                };
                (v.name.clone(), value)
            })
            .collect()
    }
}

impl Serialize for Environment {
    /// The map form when every value is static, the list form otherwise.
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        if self.is_static() {
            let mut map = s.serialize_map(Some(self.vars.len()))?;
            for v in &self.vars {
                map.serialize_entry(&v.name, v.value.as_deref().unwrap_or_default())?;
            }
            map.end()
        } else {
            let mut seq = s.serialize_seq(Some(self.vars.len()))?;
            for v in &self.vars {
                match &v.value {
                    None => seq.serialize_element(&v.name)?,
                    Some(value) => {
                        let one: BTreeMap<&str, &str> = [(v.name.as_str(), value.as_str())].into();
                        seq.serialize_element(&one)?;
                    }
                }
            }
            seq.end()
        }
    }
}

impl<'de> Deserialize<'de> for Environment {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(d)?;
        let string = |v: Value| match v {
            Value::String(s) => Ok(s),
            other => Err(de::Error::custom(format!(
                "environment values must be strings, not {other}"
            ))),
        };
        let vars = match value {
            Value::Null => Vec::new(),
            Value::Object(map) => map
                .into_iter()
                .map(|(name, v)| {
                    Ok(EnvVar {
                        name,
                        value: Some(string(v)?),
                    })
                })
                .collect::<Result<_, D::Error>>()?,
            Value::Array(items) => items
                .into_iter()
                .map(|item| match item {
                    Value::String(name) => Ok(EnvVar { name, value: None }),
                    Value::Object(map) if map.len() == 1 => {
                        let (name, v) = map.into_iter().next().unwrap_or_default();
                        Ok(EnvVar {
                            name,
                            value: Some(string(v)?),
                        })
                    }
                    Value::Object(map) => Err(de::Error::custom(format!(
                        "invalid environment variable: incorrect number of object elements (expected 1, got {})",
                        map.len()
                    ))),
                    _ => Err(de::Error::custom(
                        "invalid environment variable: unknown type",
                    )),
                })
                .collect::<Result<_, D::Error>>()?,
            _ => {
                return Err(de::Error::custom(
                    "the environment must be a map or a list",
                ));
            }
        };
        Ok(Environment { vars })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn forms() {
        let e: Environment = serde_json::from_value(json!({"B": "2", "A": "1"})).unwrap();
        assert!(e.is_static());
        assert_eq!(
            serde_json::to_value(&e).unwrap(),
            json!({"A": "1", "B": "2"})
        );

        let e: Environment = serde_json::from_value(json!(["TOKEN", {"A": "1"}, "OTHER"])).unwrap();
        assert!(!e.is_static());
        assert_eq!(e.outer_vars(), ["TOKEN", "OTHER"]);
        assert_eq!(
            serde_json::to_value(&e).unwrap(),
            json!(["TOKEN", {"A": "1"}, "OTHER"])
        );
        let outer = HashMap::from([("TOKEN".to_string(), "s3cret".to_string())]);
        let resolved = e.resolve(&outer);
        assert_eq!(resolved["TOKEN"], "s3cret");
        assert_eq!(resolved["OTHER"], "");
        assert_eq!(resolved["A"], "1");

        let e: Environment = serde_json::from_value(Value::Null).unwrap();
        assert!(e.is_empty());
        assert!(serde_json::from_value::<Environment>(json!([{"A": "1", "B": "2"}])).is_err());
    }
}
