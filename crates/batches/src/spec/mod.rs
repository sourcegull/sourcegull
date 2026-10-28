//! Bulk change specs: the YAML document that describes a bulk change.
//!
//! [`parse_batch_spec`] converts YAML (or JSON) to JSON with source positions ([`crate::yaml`]),
//! validates it against the checked-in schema ([`crate::schema::BATCH_SPEC_SCHEMA`]), deserializes
//! it and runs the semantic checks that follow schema validation ([`checks`]). Every error carries
//! the line and column it is about.
//!
//! Template strings (`${{ … }}` in `run`, `files`, `outputs`, `if`, `changesetTemplate`) are kept
//! as written; rendering them is `sourcegull-batches-template`'s job.

mod checks;
pub mod env;
pub mod published;

use std::collections::BTreeMap;
use std::fmt;

use serde::de::{self, Deserializer};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{SpecErrors, SpecIssue, sort_issues};
use crate::glob::Glob;
use crate::schema;
use crate::yaml;

pub use env::{EnvVar, Environment};
pub use published::{Published, PublishedRule, PublishedValue};

/// A parsed and validated bulk change spec.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchSpec {
    /// 1 or 2; 1 when omitted.
    #[serde(default = "default_version", deserialize_with = "de_version")]
    pub version: u32,
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub on: Vec<OnQueryOrRepository>,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub workspaces: Vec<WorkspaceConfiguration>,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub steps: Vec<Step>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transform_changes: Option<TransformChanges>,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub import_changesets: Vec<ImportChangeset>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changeset_template: Option<ChangesetTemplate>,
}

/// An entry of `on`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum OnQueryOrRepository {
    /// Every repository with a result for the search query.
    Query {
        #[serde(rename = "repositoriesMatchingQuery")]
        repositories_matching_query: String,
    },
    /// One repository, optionally on other branches than the default branch.
    Repository(OnRepository),
}

impl fmt::Display for OnQueryOrRepository {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OnQueryOrRepository::Query {
                repositories_matching_query,
            } => f.write_str(repositories_matching_query),
            OnQueryOrRepository::Repository(r) => write!(f, "repository:{}", r.repository),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OnRepository {
    pub repository: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branches: Option<Vec<String>>,
}

impl OnRepository {
    /// The branches to change; empty means the default branch. (`branch` together with
    /// `branches` is rejected by validation.)
    pub fn branches(&self) -> Vec<String> {
        match (&self.branch, &self.branches) {
            (Some(b), _) => vec![b.clone()],
            (None, Some(bs)) => bs.clone(),
            (None, None) => Vec::new(),
        }
    }
}

/// An entry of `workspaces`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceConfiguration {
    pub root_at_location_of: String,
    /// Glob of repository names; all repositories when omitted.
    #[serde(rename = "in", default, skip_serializing_if = "Option::is_none")]
    pub in_: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub only_fetch_workspace: bool,
}

impl WorkspaceConfiguration {
    /// The compiled `in` glob (validated by [`parse_batch_spec`]); `None` when omitted.
    pub fn in_glob(&self) -> Option<Result<Glob, String>> {
        self.in_.as_deref().map(Glob::new)
    }
}

/// An entry of `steps`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Step {
    pub run: String,
    pub container: String,
    #[serde(default, skip_serializing_if = "Environment::is_empty")]
    pub env: Environment,
    /// Path in the container → content (template string).
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub files: BTreeMap<String, String>,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub outputs: BTreeMap<String, Output>,
    #[serde(rename = "if", default, skip_serializing_if = "Option::is_none")]
    pub if_: Option<IfCondition>,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub mount: Vec<Mount>,
}

impl Step {
    /// The condition as a template string (`"true"`/`"false"` for booleans); `None` when the step
    /// always runs.
    pub fn if_condition(&self) -> Option<String> {
        match &self.if_ {
            None => None,
            Some(IfCondition::Bool(b)) => Some(b.to_string()),
            Some(IfCondition::Template(s)) => Some(s.clone()),
        }
    }
}

/// `steps[].if`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum IfCondition {
    Bool(bool),
    Template(String),
}

/// `steps[].outputs.<name>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Output {
    /// Template string.
    pub value: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<OutputFormat>,
}

impl Output {
    pub fn format(&self) -> OutputFormat {
        self.format.unwrap_or_default()
    }
}

/// How an output's rendered value is parsed before it is stored.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutputFormat {
    #[default]
    Text,
    Json,
    Yaml,
}

/// `steps[].mount[]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mount {
    /// Local path, relative to the bulk change spec's directory.
    pub path: String,
    /// Path in the container.
    pub mountpoint: String,
}

/// `transformChanges`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransformChanges {
    #[serde(default, deserialize_with = "null_default")]
    pub group: Vec<Group>,
}

/// `transformChanges.group[]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Group {
    pub directory: String,
    pub branch: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
}

impl TransformChanges {
    /// The groups that apply in `repo`: those without `repository` and those naming it.
    pub fn groups_for_repository(&self, repo: &str) -> Vec<&Group> {
        self.group
            .iter()
            .filter(|g| g.repository.as_deref().is_none_or(|r| r == repo))
            .collect()
    }

    /// Checks the groups of one repository: group branches are unique and differ from the
    /// changeset template's branch (compared before rendering).
    pub fn validate_groups(
        repo: &str,
        default_branch: &str,
        groups: &[&Group],
    ) -> Result<(), String> {
        let mut seen = std::collections::HashSet::new();
        for g in groups {
            if !seen.insert(g.branch.as_str()) {
                return Err(format!(
                    "transformChanges would lead to multiple changesets in repository {repo} to have the same branch {:?}",
                    g.branch
                ));
            }
            if g.branch == default_branch {
                return Err(format!(
                    "transformChanges group branch for repository {repo} is the same as branch {default_branch:?} in changesetTemplate"
                ));
            }
        }
        Ok(())
    }
}

/// `importChangesets[]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportChangeset {
    pub repository: String,
    #[serde(rename = "externalIDs", default, deserialize_with = "null_default")]
    pub external_ids: Vec<ExternalId>,
}

/// A changeset ID on the code host, written as a string or a number.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ExternalId {
    String(String),
    Number(i64),
}

impl fmt::Display for ExternalId {
    /// The ID as the code host knows it.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExternalId::String(s) => f.write_str(s),
            ExternalId::Number(n) => write!(f, "{n}"),
        }
    }
}

impl Serialize for ExternalId {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            ExternalId::String(v) => v.serialize(s),
            ExternalId::Number(n) => n.serialize(s),
        }
    }
}

impl<'de> Deserialize<'de> for ExternalId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        match Value::deserialize(d)? {
            Value::String(s) => Ok(ExternalId::String(s)),
            Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    return Ok(ExternalId::Number(i));
                }
                match n.as_f64() {
                    Some(f) if f.fract() == 0.0 && f.abs() < 9.0e15 => {
                        Ok(ExternalId::Number(f as i64))
                    }
                    _ => Err(de::Error::custom(format!("invalid external ID {n}"))),
                }
            }
            other => Err(de::Error::custom(format!(
                "cannot convert {other} into a valid external ID: expected string or int"
            ))),
        }
    }
}

/// `changesetTemplate`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChangesetTemplate {
    pub title: String,
    #[serde(default)]
    pub body: String,
    pub branch: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fork: Option<bool>,
    pub commit: CommitTemplate,
    /// `None` (omitted or null): publication is controlled from the UI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published: Option<Published>,
}

impl ChangesetTemplate {
    /// The publication flag of the changeset in `repo` on `branch`; `None` when the spec leaves
    /// it to the UI.
    pub fn published_for(&self, repo: &str, branch: &str) -> Option<PublishedValue> {
        self.published.as_ref()?.value_for_branch(repo, branch)
    }
}

/// `changesetTemplate.commit`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitTemplate {
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<CommitAuthor>,
}

/// `changesetTemplate.commit.author`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitAuthor {
    pub name: String,
    pub email: String,
}

/// A bulk change spec with the non-fatal findings of validation.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedBatchSpec {
    pub spec: BatchSpec,
    /// Things that are allowed but probably not intended (in source order).
    pub warnings: Vec<SpecIssue>,
}

impl BatchSpec {
    /// [`parse_batch_spec`] without the warnings.
    pub fn parse(text: &str) -> Result<BatchSpec, SpecErrors> {
        parse_batch_spec(text).map(|p| p.spec)
    }
}

/// Parses and validates a bulk change spec written in YAML (or JSON).
///
/// Errors: YAML syntax (including duplicate keys), schema violations, and the semantic checks of
/// [`checks`]; all of them with the line and column they are about, sorted by position.
pub fn parse_batch_spec(text: &str) -> Result<ParsedBatchSpec, SpecErrors> {
    let (value, positions) = yaml::load(text).map_err(|e| {
        SpecErrors::single(SpecIssue {
            message: e.message,
            path: String::new(),
            location: e.location,
        })
    })?;
    let mut issues = schema::validate(&schema::BATCH_SPEC, &value, &positions);
    if !issues.is_empty() {
        sort_issues(&mut issues);
        return Err(SpecErrors(issues));
    }
    let mut issues = checks::check_published(&value, &positions);
    if !issues.is_empty() {
        sort_issues(&mut issues);
        return Err(SpecErrors(issues));
    }
    let spec: BatchSpec = serde_json::from_value(value)
        .map_err(|e| SpecErrors::single(SpecIssue::new("", e.to_string())))?;
    let (mut errors, mut warnings) = checks::check(&spec, &positions);
    if !errors.is_empty() {
        sort_issues(&mut errors);
        return Err(SpecErrors(errors));
    }
    sort_issues(&mut warnings);
    Ok(ParsedBatchSpec { spec, warnings })
}

fn default_version() -> u32 {
    1
}

fn de_version<'de, D: Deserializer<'de>>(d: D) -> Result<u32, D::Error> {
    let v = Value::deserialize(d)?;
    match v.as_f64() {
        Some(f) if f == 1.0 || f == 2.0 => Ok(f as u32),
        _ => Err(de::Error::custom(format!(
            "unsupported bulk change spec version {v}"
        ))),
    }
}

fn null_default<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}
