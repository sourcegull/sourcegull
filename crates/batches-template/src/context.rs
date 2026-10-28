//! The template contexts of bulk change specs, [`StepContext`] (step `run`,
//! `env`, `files`, `outputs` and `if`) and `ChangesetTemplateContext` (the
//! `changesetTemplate` fields), including the names each exposes and what they hold when
//! empty.

use std::collections::BTreeMap;

use crate::error::TemplateError;
use crate::value::Value;
use crate::{Options, Template, is_true_output};

/// The bulk change's `name` and `description` from the spec.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BatchChangeAttributes {
    pub name: String,
    pub description: String,
}

/// The repository the steps run in.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Repository {
    pub name: String,
    pub branch: String,
    /// Paths of the search results in this repository (workspace).
    pub file_matches: Vec<String>,
}

impl Repository {
    /// `repository.search_result_paths`: the file matches, sorted.
    pub fn search_result_paths(&self) -> Vec<String> {
        let mut p = self.file_matches.clone();
        p.sort();
        p
    }

    fn names_value(&self) -> Value {
        Value::map([
            ("name", Value::from(self.name.as_str())),
            ("branch", Value::from(self.branch.as_str())),
            (
                "search_result_paths",
                Value::PathList(self.search_result_paths()),
            ),
        ])
    }

    fn go_value(&self) -> Value {
        Value::map([
            ("Name", Value::from(self.name.as_str())),
            ("Branch", Value::from(self.branch.as_str())),
            (
                "FileMatches",
                Value::strings(self.file_matches.iter().cloned()),
            ),
            (
                "SearchResultPaths",
                Value::PathList(self.search_result_paths()),
            ),
        ])
    }
}

impl BatchChangeAttributes {
    fn names_value(&self) -> Value {
        Value::map([
            ("name", Value::from(self.name.as_str())),
            ("description", Value::from(self.description.as_str())),
        ])
    }

    fn go_value(&self) -> Value {
        Value::map([
            ("Name", Value::from(self.name.as_str())),
            ("Description", Value::from(self.description.as_str())),
        ])
    }
}

/// Files changed by steps.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Changes {
    pub modified: Vec<String>,
    pub added: Vec<String>,
    pub deleted: Vec<String>,
    pub renamed: Vec<String>,
}

impl Changes {
    fn go_value(&self) -> Value {
        Value::map([
            ("Modified", Value::strings(self.modified.iter().cloned())),
            ("Added", Value::strings(self.added.iter().cloned())),
            ("Deleted", Value::strings(self.deleted.iter().cloned())),
            ("Renamed", Value::strings(self.renamed.iter().cloned())),
        ])
    }

    fn insert_names(&self, m: &mut BTreeMap<String, Value>) {
        m.insert(
            "modified_files".into(),
            Value::strings(self.modified.iter().cloned()),
        );
        m.insert(
            "added_files".into(),
            Value::strings(self.added.iter().cloned()),
        );
        m.insert(
            "deleted_files".into(),
            Value::strings(self.deleted.iter().cloned()),
        );
        m.insert(
            "renamed_files".into(),
            Value::strings(self.renamed.iter().cloned()),
        );
    }
}

/// The result of a step (the parts templates see).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StepResult {
    pub changed_files: Changes,
    pub stdout: String,
    pub stderr: String,
}

impl StepResult {
    fn names_value(&self) -> Value {
        let mut m = BTreeMap::new();
        self.changed_files.insert_names(&mut m);
        m.insert("stdout".into(), Value::from(self.stdout.as_str()));
        m.insert("stderr".into(), Value::from(self.stderr.as_str()));
        Value::Map(m)
    }

    fn go_value(&self) -> Value {
        Value::map([
            ("ChangedFiles", self.changed_files.go_value()),
            ("Stdout", Value::from(self.stdout.as_str())),
            ("Stderr", Value::from(self.stderr.as_str())),
        ])
    }
}

/// The workspace path and the changes of all steps so far.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StepsContext {
    pub changes: Changes,
    /// The workspace directory relative to the repository root; `""` for the root, no
    /// leading `/`.
    pub path: String,
}

impl StepsContext {
    fn go_value(&self) -> Value {
        Value::map([
            ("Changes", self.changes.go_value()),
            ("Path", Value::from(self.path.as_str())),
        ])
    }
}

/// What a step's fields (`run`, `env`, `files`, `outputs`, `if`) can refer to.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StepContext {
    pub batch_change: BatchChangeAttributes,
    /// Outputs set by the current and all previous steps.
    pub outputs: BTreeMap<String, Value>,
    /// The current step's result: empty while rendering `run`, filled for `outputs`.
    pub step: StepResult,
    pub steps: StepsContext,
    /// The previous step's result; empty for the first step.
    pub previous_step: StepResult,
    pub repository: Repository,
}

impl StepContext {
    /// The context names (`repository`, `batch_change`, `outputs`, `steps`, `step`,
    /// `previous_step`).
    pub fn names(&self) -> Value {
        let mut steps = BTreeMap::new();
        self.steps.changes.insert_names(&mut steps);
        steps.insert("stdout".into(), Value::from(""));
        steps.insert("stderr".into(), Value::from(""));
        steps.insert("path".into(), Value::from(self.steps.path.as_str()));
        Value::map([
            ("batch_change", self.batch_change.names_value()),
            ("repository", self.repository.names_value()),
            ("outputs", Value::Map(self.outputs.clone())),
            ("step", self.step.names_value()),
            ("previous_step", self.previous_step.names_value()),
            ("steps", Value::Map(steps)),
        ])
    }

    /// The dot: a struct-like map (`.Repository.Name`, `.BatchChange.Name`,
    /// `.Steps.Path`, `.Outputs.x`, …), plus the lower-case names so that
    /// `.repository.name` works too.
    pub fn dot(&self) -> Value {
        let mut m = match self.names() {
            Value::Map(m) => m,
            _ => BTreeMap::new(),
        };
        m.insert("BatchChange".into(), self.batch_change.go_value());
        m.insert("Outputs".into(), Value::Map(self.outputs.clone()));
        m.insert("Step".into(), self.step.go_value());
        m.insert("Steps".into(), self.steps.go_value());
        m.insert("PreviousStep".into(), self.previous_step.go_value());
        m.insert("Repository".into(), self.repository.go_value());
        Value::Map(m)
    }

    /// Renders a step field.
    pub fn render(&self, template: &str) -> Result<String, TemplateError> {
        Template::parse(template)?.execute(&self.names(), &self.dot(), &Options::default())
    }

    /// Renders each value of a map (`env`, `files`).
    pub fn render_map(
        &self,
        m: &BTreeMap<String, String>,
    ) -> Result<BTreeMap<String, String>, TemplateError> {
        let names = self.names();
        let dot = self.dot();
        let opts = Options::default();
        m.iter()
            .map(|(k, v)| {
                Template::parse(v)?
                    .execute(&names, &dot, &opts)
                    .map(|r| (k.clone(), r))
            })
            .collect()
    }

    /// Evaluates a step's `if:`: empty is true, otherwise
    /// the rendered text, trimmed, must be `true`.
    pub fn eval_condition(&self, condition: &str) -> Result<bool, TemplateError> {
        if condition.is_empty() {
            return Ok(true);
        }
        Ok(is_true_output(&self.render(condition)?))
    }
}

/// What `changesetTemplate` fields (`title`, `body`, `branch`, `commit.message`, …) can
/// refer to.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ChangesetTemplateContext {
    pub batch_change_attributes: BatchChangeAttributes,
    pub steps: StepsContext,
    pub outputs: BTreeMap<String, Value>,
    pub repository: Repository,
}

/// `batch_change_link` renders to itself; the reconciler fills it in when publishing.
pub const BATCH_CHANGE_LINK: &str = "${{ batch_change_link }}";

impl ChangesetTemplateContext {
    /// The context names (`repository`, `batch_change`, `outputs`, `steps`,
    /// `batch_change_link`).
    pub fn names(&self) -> Value {
        let mut steps = BTreeMap::new();
        self.steps.changes.insert_names(&mut steps);
        steps.insert("path".into(), Value::from(self.steps.path.as_str()));
        Value::map([
            ("repository", self.repository.names_value()),
            ("batch_change", self.batch_change_attributes.names_value()),
            ("outputs", Value::Map(self.outputs.clone())),
            ("steps", Value::Map(steps)),
            ("batch_change_link", Value::from(BATCH_CHANGE_LINK)),
        ])
    }

    /// The dot: the Go struct's fields plus the lower-case names.
    pub fn dot(&self) -> Value {
        let mut m = match self.names() {
            Value::Map(m) => m,
            _ => BTreeMap::new(),
        };
        m.insert(
            "BatchChangeAttributes".into(),
            self.batch_change_attributes.go_value(),
        );
        m.insert("Steps".into(), self.steps.go_value());
        m.insert("Outputs".into(), Value::Map(self.outputs.clone()));
        m.insert("Repository".into(), self.repository.go_value());
        Value::Map(m)
    }

    /// Renders a changeset template field and trims surrounding whitespace.
    pub fn render_field(&self, template: &str) -> Result<String, TemplateError> {
        let out =
            Template::parse(template)?.execute(&self.names(), &self.dot(), &Options::default())?;
        Ok(out.trim().to_owned())
    }
}
