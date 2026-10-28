//! Changeset specs: what one changeset should look like, as uploaded by executors and
//! `gull bulk` (`createChangesetSpec(s)`), or generated for `importChangesets`.
//!
//! In the JSON form, commits with `version: 2` carry the diff base64-encoded (so binary diffs
//! survive), other versions carry it as text.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::de::{self, Deserializer};
use serde::ser::Serializer;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{SpecErrors, SpecIssue, sort_issues};
use crate::schema;
use crate::spec::PublishedValue;
use crate::yaml::{self, Positions};

/// The error when the head repository is not the base repository.
pub const HEAD_BASE_MISMATCH: &str = "headRepository does not match baseRepository";

/// A changeset spec: a new changeset from a branch, or an existing changeset to import.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum ChangesetSpec {
    Existing(ExistingChangesetSpec),
    Branch(BranchChangesetSpec),
}

impl<'de> Deserialize<'de> for ChangesetSpec {
    /// The shape is chosen by the presence of `externalID`, as in the schema.
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = Value::deserialize(d)?;
        let existing = v.get("externalID").is_some();
        if existing {
            serde_json::from_value(v).map(ChangesetSpec::Existing)
        } else {
            serde_json::from_value(v).map(ChangesetSpec::Branch)
        }
        .map_err(de::Error::custom)
    }
}

/// Imports the changeset `external_id` of `base_repository`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExistingChangesetSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<i64>,
    /// GraphQL ID of the repository.
    pub base_repository: String,
    #[serde(rename = "externalID")]
    pub external_id: String,
}

/// A changeset whose commit is pushed to `head_ref`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BranchChangesetSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<i64>,
    /// GraphQL ID of the target repository.
    pub base_repository: String,
    /// `refs/heads/<target branch>`.
    pub base_ref: String,
    /// The commit of `base_ref` the diff applies to.
    pub base_rev: String,
    /// Equal to `base_repository` (forks are requested with `fork`).
    pub head_repository: String,
    /// `refs/heads/<changeset branch>`.
    pub head_ref: String,
    pub title: String,
    pub body: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fork: Option<bool>,
    /// Exactly one commit.
    pub commits: Vec<GitCommitDescription>,
    /// `None`: publication is controlled from the UI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published: Option<PublishedValue>,
}

impl BranchChangesetSpec {
    /// The (only) commit.
    pub fn commit(&self) -> Option<&GitCommitDescription> {
        self.commits.first()
    }

    /// The changeset branch without `refs/heads/`.
    pub fn head_branch(&self) -> &str {
        self.head_ref
            .strip_prefix("refs/heads/")
            .unwrap_or(&self.head_ref)
    }

    /// The target branch without `refs/heads/`.
    pub fn base_branch(&self) -> &str {
        self.base_ref
            .strip_prefix("refs/heads/")
            .unwrap_or(&self.base_ref)
    }
}

/// A commit of a changeset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitCommitDescription {
    /// 2: the diff is base64 on the wire; anything else: text (non-UTF-8 bytes are replaced when
    /// serializing).
    pub version: i64,
    pub message: String,
    /// Unified diff.
    pub diff: Vec<u8>,
    pub author_name: String,
    pub author_email: String,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireCommit {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    version: Option<i64>,
    message: String,
    diff: String,
    author_name: String,
    author_email: String,
}

impl Serialize for GitCommitDescription {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let (version, diff) = if self.version == 2 {
            (Some(2), BASE64.encode(&self.diff))
        } else {
            (None, String::from_utf8_lossy(&self.diff).into_owned())
        };
        WireCommit {
            version,
            message: self.message.clone(),
            diff,
            author_name: self.author_name.clone(),
            author_email: self.author_email.clone(),
        }
        .serialize(s)
    }
}

impl<'de> Deserialize<'de> for GitCommitDescription {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let w = WireCommit::deserialize(d)?;
        let version = w.version.unwrap_or(0);
        let diff = if version == 2 {
            BASE64
                .decode(w.diff.as_bytes())
                .map_err(|e| de::Error::custom(format!("commit diff is not valid base64: {e}")))?
        } else {
            w.diff.into_bytes()
        };
        Ok(GitCommitDescription {
            version,
            message: w.message,
            diff,
            author_name: w.author_name,
            author_email: w.author_email,
        })
    }
}

impl ChangesetSpec {
    /// [`parse_changeset_spec`].
    pub fn parse(text: &str) -> Result<ChangesetSpec, SpecErrors> {
        parse_changeset_spec(text)
    }

    /// Validates an already parsed JSON value (positions are then unknown).
    pub fn from_value(value: Value) -> Result<ChangesetSpec, SpecErrors> {
        validate_value(value, Positions::default)
    }

    /// The JSON form.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    /// GraphQL ID of the target repository.
    pub fn base_repository(&self) -> &str {
        match self {
            ChangesetSpec::Existing(e) => &e.base_repository,
            ChangesetSpec::Branch(b) => &b.base_repository,
        }
    }

    pub fn is_importing_existing(&self) -> bool {
        matches!(self, ChangesetSpec::Existing(_))
    }

    pub fn as_branch(&self) -> Option<&BranchChangesetSpec> {
        match self {
            ChangesetSpec::Branch(b) => Some(b),
            ChangesetSpec::Existing(_) => None,
        }
    }
}

/// Parses and validates a changeset spec (JSON): the schema, a decodable diff, and
/// `headRepository` equal to `baseRepository`. Errors carry the line and column.
pub fn parse_changeset_spec(text: &str) -> Result<ChangesetSpec, SpecErrors> {
    let value: Value = serde_json::from_str(text).map_err(|e| {
        let location = (e.line() > 0).then_some(yaml::Location {
            line: e.line(),
            column: e.column(),
        });
        SpecErrors::single(SpecIssue::new("", e.to_string()).at(location))
    })?;
    // Positions are only needed for errors; JSON is YAML, so the YAML loader finds them.
    validate_value(value, || {
        yaml::load(text).map(|(_, p)| p).unwrap_or_default()
    })
}

fn validate_value(
    value: Value,
    positions: impl FnOnce() -> Positions,
) -> Result<ChangesetSpec, SpecErrors> {
    let none = Positions::default();
    if !schema::validate(&schema::CHANGESET_SPEC, &value, &none).is_empty() {
        // Again with positions, which cost a second parse of the text.
        let mut issues = schema::validate(&schema::CHANGESET_SPEC, &value, &positions());
        sort_issues(&mut issues);
        return Err(SpecErrors(issues));
    }
    let spec: ChangesetSpec = serde_json::from_value(value)
        .map_err(|e| SpecErrors::single(SpecIssue::new("", e.to_string())))?;
    if let ChangesetSpec::Branch(b) = &spec {
        if b.head_repository != b.base_repository {
            let positions = positions();
            return Err(SpecErrors::single(
                SpecIssue::new("/headRepository", HEAD_BASE_MISMATCH)
                    .at(positions.value("/headRepository")),
            ));
        }
    }
    Ok(spec)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn head_branch() {
        let spec = BranchChangesetSpec {
            version: None,
            base_repository: "R".into(),
            base_ref: "refs/heads/main".into(),
            base_rev: "abc".into(),
            head_repository: "R".into(),
            head_ref: "refs/heads/fix".into(),
            title: "t".into(),
            body: "b".into(),
            fork: None,
            commits: vec![],
            published: None,
        };
        assert_eq!(spec.head_branch(), "fix");
        assert_eq!(spec.base_branch(), "main");
    }
}
