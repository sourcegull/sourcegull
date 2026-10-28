//! Index configuration: the jobs to run for a repository, as written in `sourcegull.yaml` at the
//! repository root, stored per repository through the UI (JSON), or inferred by the recognizers.

use serde::{Deserialize, Serialize};

/// The configuration file names read at a repository's root, in order.
pub const CONFIG_FILES: [&str; 1] = ["sourcegull.yaml"];

/// `{"index_jobs": [...]}`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexConfiguration {
    #[serde(default, deserialize_with = "null_as_default")]
    pub index_jobs: Vec<IndexJobSpec>,
}

/// One index job: optional pre-index steps (each in its own container), then the indexer.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexJobSpec {
    /// Steps run before the indexer, each in its own container.
    #[serde(default, deserialize_with = "null_as_default")]
    pub steps: Vec<DockerStep>,
    /// Commands run in the indexer's container before the indexer.
    #[serde(default, deserialize_with = "null_as_default")]
    pub local_steps: Vec<String>,
    /// Directory the indexer runs in, relative to the repository root (`""` for the root).
    #[serde(default, deserialize_with = "null_as_default")]
    pub root: String,
    /// The indexer's container image.
    #[serde(default, deserialize_with = "null_as_default")]
    pub indexer: String,
    /// The indexer's command line.
    #[serde(default, deserialize_with = "null_as_default")]
    pub indexer_args: Vec<String>,
    /// The index file the indexer writes, relative to `root`.
    #[serde(default, deserialize_with = "null_as_default")]
    pub outfile: String,
    /// Names of executor secrets passed to the steps as environment variables.
    #[serde(
        default,
        rename = "requestedEnvVars",
        deserialize_with = "null_as_default"
    )]
    pub requested_env_vars: Vec<String>,
}

/// A step run in its own container before the indexer.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DockerStep {
    #[serde(default, deserialize_with = "null_as_default")]
    pub root: String,
    #[serde(default, deserialize_with = "null_as_default")]
    pub image: String,
    #[serde(default, deserialize_with = "null_as_default")]
    pub commands: Vec<String>,
}

fn null_as_default<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

/// A configuration that does not parse.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ConfigError(pub String);

impl IndexConfiguration {
    /// Parses JSON with comments and trailing commas; empty text is an empty configuration.
    pub fn from_json(text: &str) -> Result<Self, ConfigError> {
        let stripped = sourcegull_config::strip_jsonc(text)
            .map_err(|e| ConfigError(format!("invalid JSON: {e}")))?;
        if stripped.trim().is_empty() {
            return Ok(Self::default());
        }
        serde_json::from_str(&stripped).map_err(|e| ConfigError(format!("invalid JSON: {e}")))
    }

    /// Parses a `sourcegull.yaml` file.
    pub fn from_yaml(text: &str) -> Result<Self, ConfigError> {
        if text.trim().is_empty() {
            return Ok(Self::default());
        }
        serde_yaml_ng::from_str(text).map_err(|e| ConfigError(format!("invalid YAML: {e}")))
    }

    /// Pretty JSON, with empty lists rather than nulls.
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".into())
    }

    /// The jobs with a usable indexer (jobs without one are dropped).
    pub fn into_jobs(self) -> Vec<IndexJobSpec> {
        self.index_jobs
            .into_iter()
            .filter(|j| !j.indexer.trim().is_empty())
            .collect()
    }
}

/// The bare indexer name of an image: `sourcegraph/scip-go@sha256:…` → `scip-go` (the
/// `sourcegraph/` prefix and a digest are removed).
pub fn indexer_name(image: &str) -> &str {
    let name = image.strip_prefix("sourcegraph/").unwrap_or(image);
    match name.rfind('@') {
        Some(i) => &name[..i],
        None => name,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_round_trip_with_comments() {
        let c = IndexConfiguration::from_json(
            r#"{
                // a comment
                "index_jobs": [
                    {"indexer": "sourcegraph/scip-go", "root": "cmd", "steps": [
                        {"image": "golang", "commands": ["go mod download"]},
                    ], "requestedEnvVars": ["GOPRIVATE"], "indexer_args": null},
                ],
            }"#,
        )
        .unwrap();
        assert_eq!(c.index_jobs.len(), 1);
        let j = &c.index_jobs[0];
        assert_eq!(j.root, "cmd");
        assert_eq!(j.steps[0].commands, ["go mod download"]);
        assert_eq!(j.requested_env_vars, ["GOPRIVATE"]);
        assert!(j.indexer_args.is_empty());
        let json = c.to_json();
        assert!(json.contains("\"local_steps\": []"), "{json}");
        assert_eq!(IndexConfiguration::from_json(&json).unwrap(), c);
        assert_eq!(
            IndexConfiguration::from_json("  ").unwrap(),
            IndexConfiguration::default()
        );
        assert!(IndexConfiguration::from_json("{").is_err());
    }

    #[test]
    fn yaml() {
        let c = IndexConfiguration::from_yaml(
            "index_jobs:\n  - indexer: sourcegraph/scip-typescript\n    indexer_args: [scip-typescript, index]\n    local_steps:\n      - npm install\n  - root: no-indexer\n",
        )
        .unwrap();
        assert_eq!(c.index_jobs[0].indexer_args, ["scip-typescript", "index"]);
        assert_eq!(c.clone().into_jobs().len(), 1);
        assert!(IndexConfiguration::from_yaml("index_jobs: 3").is_err());
    }

    #[test]
    fn indexer_names() {
        assert_eq!(indexer_name("sourcegraph/scip-go@sha256:abc"), "scip-go");
        assert_eq!(indexer_name("sourcegraph/scip-go"), "scip-go");
        assert_eq!(indexer_name("acme/custom:1"), "acme/custom:1");
    }
}
