//! Step results: what a step leaves behind, as templates see it and as the cache stores it.
//!
//! A result holds the cumulative diff and changed files of the workspace after the step, the
//! step's stdout and stderr, and every output set so far.

use std::collections::BTreeMap;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sourcegull_batches_template::{self as template, Value};

/// Format version of [`AfterStepResult`]'s serialized form.
pub const RESULT_VERSION: u32 = 2;

/// Files changed in the workspace relative to the base commit,
/// from `git status --porcelain` after `git add --all`, in git's (path) order.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChangedFiles {
    #[serde(default)]
    pub modified: Vec<String>,
    #[serde(default)]
    pub added: Vec<String>,
    #[serde(default)]
    pub deleted: Vec<String>,
    /// The new names of renamed files.
    #[serde(default)]
    pub renamed: Vec<String>,
}

impl ChangedFiles {
    pub fn is_empty(&self) -> bool {
        self.modified.is_empty()
            && self.added.is_empty()
            && self.deleted.is_empty()
            && self.renamed.is_empty()
    }

    /// The template view (`steps.modified_files`, …).
    pub fn to_template(&self) -> template::Changes {
        template::Changes {
            modified: self.modified.clone(),
            added: self.added.clone(),
            deleted: self.deleted.clone(),
            renamed: self.renamed.clone(),
        }
    }
}

/// The state of a workspace after one step ran. Serializable, so it can be cached
/// ([`crate::StepCache`]) and resumed from: applying [`diff`](Self::diff) to the base commit
/// restores the workspace, and [`outputs`](Self::outputs) restores the template context.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AfterStepResult {
    /// [`RESULT_VERSION`].
    #[serde(default)]
    pub version: u32,
    /// The step (from 0) this is the result of.
    pub step_index: usize,
    /// The workspace's diff against the base commit after the step (`git diff --cached
    /// --no-prefix --binary`); base64 when serialized.
    #[serde(with = "base64_bytes")]
    pub diff: Vec<u8>,
    /// The workspace's changed files after the step (cumulative, like the diff).
    #[serde(default)]
    pub changed_files: ChangedFiles,
    #[serde(default)]
    pub stdout: String,
    #[serde(default)]
    pub stderr: String,
    /// Every output set by this step and the steps before it.
    #[serde(default, with = "outputs_json")]
    pub outputs: BTreeMap<String, Value>,
}

impl AfterStepResult {
    /// The template view (`previous_step.*`, `step.*`).
    pub fn to_template(&self) -> template::StepResult {
        template::StepResult {
            changed_files: self.changed_files.to_template(),
            stdout: self.stdout.clone(),
            stderr: self.stderr.clone(),
        }
    }

    /// The JSON form (what [`crate::StepCache`] implementations store).
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    pub fn from_json(text: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(text)
    }
}

mod base64_bytes {
    use super::*;

    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        BASE64.encode(bytes).serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let text = String::deserialize(d)?;
        BASE64
            .decode(text.as_bytes())
            .map_err(|e| serde::de::Error::custom(format!("diff is not valid base64: {e}")))
    }
}

/// Outputs as JSON. Integers stay integers and floats keep their fraction (`3.0`), so a
/// `format: yaml` integer and a `format: json` number (a float) survive a round trip with
/// their types.
mod outputs_json {
    use super::*;

    pub fn serialize<S: Serializer>(m: &BTreeMap<String, Value>, s: S) -> Result<S::Ok, S::Error> {
        let json: serde_json::Map<String, serde_json::Value> =
            m.iter().map(|(k, v)| (k.clone(), v.to_json())).collect();
        json.serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<BTreeMap<String, Value>, D::Error> {
        let json = BTreeMap::<String, serde_json::Value>::deserialize(d)?;
        Ok(json
            .into_iter()
            .map(|(k, v)| (k, Value::from_json_yaml_numbers(&v)))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_keeps_go_types() {
        let r = AfterStepResult {
            version: RESULT_VERSION,
            step_index: 2,
            diff: b"diff --git a a\n\0binary".to_vec(),
            changed_files: ChangedFiles {
                modified: vec!["a".into()],
                ..Default::default()
            },
            stdout: "out".into(),
            stderr: String::new(),
            outputs: [
                ("int".to_owned(), Value::Int(3)),
                ("float".to_owned(), Value::Float(3.0)),
                ("text".to_owned(), Value::from("x")),
                (
                    "list".to_owned(),
                    Value::List(vec![Value::Int(1), Value::from("a")]),
                ),
            ]
            .into(),
        };
        let back = AfterStepResult::from_json(&r.to_json()).unwrap();
        assert_eq!(back, r);
    }
}
