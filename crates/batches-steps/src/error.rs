use sourcegull_batches_template::TemplateError;

use crate::git::GitError;
use crate::runtime::RuntimeError;

/// Why running steps or building changeset specs failed. Steps are numbered from 0.
#[derive(Debug, thiserror::Error)]
pub enum StepsError {
    /// A step field (`if`, `run`, `env.<name>`, `files.<path>`, `outputs.<name>`) failed to
    /// render.
    #[error("step {}: rendering {field}: {error}", step + 1)]
    Template {
        step: usize,
        field: String,
        error: Box<TemplateError>,
    },
    /// A rendered output could not be parsed in its `format`.
    #[error("step {}: output {name}: {message}", step + 1)]
    Output {
        step: usize,
        name: String,
        message: String,
    },
    /// The step's script exited with a non-zero status.
    #[error("step {} failed with exit code {exit_code}", step + 1)]
    StepFailed {
        step: usize,
        exit_code: i32,
        stdout: String,
        stderr: String,
    },
    /// A `mount` or `files` path is not allowed (outside the mount root, relative target, …).
    #[error("step {}: {message}", step + 1)]
    InvalidPath { step: usize, message: String },
    #[error("step {}: {error}", step + 1)]
    Runtime { step: usize, error: RuntimeError },
    /// Resolving (pulling) an image failed.
    #[error(transparent)]
    Image(RuntimeError),
    #[error(transparent)]
    Git(#[from] GitError),
    /// A `changesetTemplate` field failed to render.
    #[error("rendering changesetTemplate.{field}: {error}")]
    ChangesetTemplate {
        field: String,
        error: Box<TemplateError>,
    },
    #[error("{0}")]
    TransformChanges(String),
    #[error("canceled")]
    Canceled,
    #[error("{0}")]
    Io(String),
}

impl StepsError {
    /// The step the error is about, if any.
    pub fn step(&self) -> Option<usize> {
        match self {
            StepsError::Template { step, .. }
            | StepsError::Output { step, .. }
            | StepsError::StepFailed { step, .. }
            | StepsError::InvalidPath { step, .. }
            | StepsError::Runtime { step, .. } => Some(*step),
            _ => None,
        }
    }
}
