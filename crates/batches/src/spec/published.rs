//! `changesetTemplate.published`: a flag for every changeset, or rules by repository glob where
//! the last matching rule wins. A `glob@branch` suffix limits a rule to one changeset branch.

use serde::de::{self, Deserializer};
use serde::ser::{SerializeMap, SerializeSeq, Serializer};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::glob::Glob;

/// A publication flag: `true`, `false` or `"draft"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PublishedValue {
    Published,
    Unpublished,
    Draft,
}

impl PublishedValue {
    /// From the JSON forms `true`, `false` and `"draft"`.
    pub fn from_json(v: &Value) -> Option<Self> {
        match v {
            Value::Bool(true) => Some(PublishedValue::Published),
            Value::Bool(false) => Some(PublishedValue::Unpublished),
            Value::String(s) if s == "draft" => Some(PublishedValue::Draft),
            _ => None,
        }
    }

    pub fn to_json(self) -> Value {
        match self {
            PublishedValue::Published => Value::Bool(true),
            PublishedValue::Unpublished => Value::Bool(false),
            PublishedValue::Draft => Value::String("draft".into()),
        }
    }
}

impl Serialize for PublishedValue {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.to_json().serialize(s)
    }
}

impl<'de> Deserialize<'de> for PublishedValue {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = Value::deserialize(d)?;
        PublishedValue::from_json(&v).ok_or_else(|| {
            de::Error::custom(format!(
                "invalid published value {v}: expected true, false or \"draft\""
            ))
        })
    }
}

/// One `{glob: flag}` rule.
#[derive(Debug, Clone, PartialEq)]
pub struct PublishedRule {
    /// The key as written, `glob` or `glob@branch`.
    pub pattern: String,
    glob: Glob,
    /// The part after `@`: the rule only applies to changesets on this branch.
    pub branch: Option<String>,
    pub value: PublishedValue,
}

impl PublishedRule {
    pub fn new(pattern: &str, value: PublishedValue) -> Result<Self, String> {
        let (glob, branch) = match pattern.split_once('@') {
            Some((g, b)) => (g, Some(b.to_string())),
            None => (pattern, None),
        };
        Ok(PublishedRule {
            pattern: pattern.to_string(),
            glob: Glob::new(glob)?,
            branch,
            value,
        })
    }

    fn matches(&self, repo: &str, branch: Option<&str>) -> bool {
        self.glob.is_match(repo)
            && match (&self.branch, branch) {
                (None, _) => true,
                (Some(want), Some(have)) => want == have,
                // Without a branch the suffix is ignored.
                (Some(_), None) => true,
            }
    }
}

/// The `published` field of a changeset template.
#[derive(Debug, Clone, PartialEq)]
pub enum Published {
    /// One flag for every changeset.
    All(PublishedValue),
    /// Rules by repository name; the last match wins and no match means "controlled from the
    /// UI".
    Rules(Vec<PublishedRule>),
}

impl Published {
    /// The flag for changesets in `repo`, ignoring `@branch` suffixes.
    pub fn value_for(&self, repo: &str) -> Option<PublishedValue> {
        self.lookup(repo, None)
    }

    /// The flag for the changeset of `repo` on `branch` (the rendered `changesetTemplate.branch`
    /// or a `transformChanges` group branch, without `refs/heads/`).
    pub fn value_for_branch(&self, repo: &str, branch: &str) -> Option<PublishedValue> {
        self.lookup(repo, Some(branch))
    }

    fn lookup(&self, repo: &str, branch: Option<&str>) -> Option<PublishedValue> {
        match self {
            Published::All(v) => Some(*v),
            Published::Rules(rules) => rules
                .iter()
                .rev()
                .find(|r| r.matches(repo, branch))
                .map(|r| r.value),
        }
    }

    /// Parses the JSON form; errors name the rule index (from 0) and the problem.
    pub fn from_json(v: &Value) -> Result<Self, (Option<usize>, String)> {
        if let Some(flag) = PublishedValue::from_json(v) {
            return Ok(Published::All(flag));
        }
        let Value::Array(items) = v else {
            return Err((
                None,
                "expected true, false, \"draft\" or a list of rules".into(),
            ));
        };
        let mut rules = Vec::with_capacity(items.len());
        for (i, item) in items.iter().enumerate() {
            let rule = match item {
                Value::Object(map) if map.len() == 1 => {
                    let (pattern, flag) = map.iter().next().ok_or((Some(i), String::new()))?;
                    let flag = PublishedValue::from_json(flag).ok_or_else(|| {
                        (Some(i), "expected true, false or \"draft\"".to_string())
                    })?;
                    PublishedRule::new(pattern, flag).map_err(|e| (Some(i), e))?
                }
                Value::Object(map) => {
                    return Err((
                        Some(i),
                        format!(
                            "unexpected number of elements in the array at entry {i}: {} (must be 1)",
                            map.len()
                        ),
                    ));
                }
                _ => return Err((Some(i), "expected a {glob: flag} object".into())),
            };
            rules.push(rule);
        }
        Ok(Published::Rules(rules))
    }
}

impl Serialize for Published {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Published::All(v) => v.serialize(s),
            Published::Rules(rules) => {
                let mut seq = s.serialize_seq(Some(rules.len()))?;
                for r in rules {
                    seq.serialize_element(&OneRule(r))?;
                }
                seq.end()
            }
        }
    }
}

struct OneRule<'a>(&'a PublishedRule);

impl Serialize for OneRule<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut map = s.serialize_map(Some(1))?;
        map.serialize_entry(&self.0.pattern, &self.0.value)?;
        map.end()
    }
}

impl<'de> Deserialize<'de> for Published {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = Value::deserialize(d)?;
        Published::from_json(&v).map_err(|(_, e)| de::Error::custom(e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use PublishedValue as V;
    use serde_json::json;

    #[test]
    fn rules_last_match_wins() {
        let p = Published::from_json(&json!([
            {"*": false},
            {"gitlab.com/group/*": "draft"},
            {"gitlab.com/group/special": true},
            {"gitlab.com/other@fix-branch": true},
        ]))
        .unwrap();
        assert_eq!(p.value_for("gitlab.com/x/y"), Some(V::Unpublished));
        assert_eq!(p.value_for("gitlab.com/group/a"), Some(V::Draft));
        assert_eq!(p.value_for("gitlab.com/group/special"), Some(V::Published));
        assert_eq!(p.value_for("gitlab.com/other"), Some(V::Published));
        assert_eq!(
            p.value_for_branch("gitlab.com/other", "fix-branch"),
            Some(V::Published)
        );
        assert_eq!(
            p.value_for_branch("gitlab.com/other", "else"),
            Some(V::Unpublished)
        );

        let none = Published::from_json(&json!([{"gitlab.com/a": true}])).unwrap();
        assert_eq!(none.value_for("gitlab.com/b"), None);

        let all = Published::from_json(&json!("draft")).unwrap();
        assert_eq!(all.value_for("anything"), Some(V::Draft));
        assert_eq!(serde_json::to_value(&all).unwrap(), json!("draft"));
        assert_eq!(
            serde_json::to_value(&p).unwrap()[3],
            json!({"gitlab.com/other@fix-branch": true})
        );
    }

    #[test]
    fn invalid() {
        assert!(Published::from_json(&json!("yes")).is_err());
        assert_eq!(
            Published::from_json(&json!([{"a": true}, {"[": true}]))
                .unwrap_err()
                .0,
            Some(1)
        );
        assert!(Published::from_json(&json!([{"a": true, "b": false}])).is_err());
    }
}
